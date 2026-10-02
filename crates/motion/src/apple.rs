//! The macOS decoder: AVAssetReader hands back hardware-decoded, IOSurface-backed pixel buffers
//! in the bi-planar 4:2:0 layout gpui's `surface` element draws without a copy, or the same
//! planes copied out into memory when planes are asked for.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use core_foundation::base::TCFType as _;
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef};
use futures::SinkExt as _;
use futures::channel::mpsc;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::CFString;
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use crate::{Frame, Picture};

/// Opens `path` once on the caller's thread, so a file the system cannot read fails here, then
/// decodes it forever on a thread of its own.
pub(crate) fn spawn(path: &Path, edge: u32, sender: mpsc::Sender<Frame>) -> Result<()> {
    let path = path.to_path_buf();
    let planes = crate::planes();
    autoreleasepool(|_| reader(&path, edge, planes).map(drop))?;
    std::thread::Builder::new()
        .name("motion-decoder".into())
        .spawn(move || {
            if let Err(error) = decode(&path, edge, planes, sender) {
                log::warn!("motion: cannot decode {}: {error:#}", path.display());
            }
        })
        .context("cannot start the motion decoder thread")?;
    Ok(())
}

/// Reads the file from the start each time it runs out, until the receiver goes away.
fn decode(path: &Path, edge: u32, planes: bool, mut sender: mpsc::Sender<Frame>) -> Result<()> {
    loop {
        let finished = autoreleasepool(|_| -> Result<bool> {
            let (reader, output) = reader(path, edge, planes)?;
            let mut sent = 0usize;
            while let Some(sample) = unsafe { output.copyNextSampleBuffer() } {
                let Some(image) = (unsafe { sample.image_buffer() }) else {
                    continue;
                };
                let time = unsafe { sample.presentation_time_stamp() };
                let pts = match time.timescale {
                    scale if scale > 0 => {
                        Duration::from_secs_f64(time.value.max(0) as f64 / scale as f64)
                    }
                    _ => Duration::ZERO,
                };
                let picture = match planes {
                    true => nv12(&image)?,
                    false => {
                        let raw = objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr()
                            as CVPixelBufferRef;
                        Picture::Surface(unsafe { CVPixelBuffer::wrap_under_get_rule(raw) })
                    }
                };
                if futures::executor::block_on(sender.send(Frame { picture, pts })).is_err() {
                    unsafe { reader.cancelReading() };
                    return Ok(true);
                }
                sent += 1;
            }
            if unsafe { reader.status() } == AVAssetReaderStatus::Failed {
                bail!("the asset reader failed");
            }
            if sent == 0 {
                bail!("the file has no video frames");
            }
            Ok(false)
        })?;
        if finished {
            return Ok(());
        }
    }
}

/// A started reader over the file's first video track, asking for buffers scaled to `edge`. With
/// `planes` it asks for the video range, which is what decoders elsewhere hand out too.
fn reader(
    path: &Path,
    edge: u32,
    planes: bool,
) -> Result<(Retained<AVAssetReader>, Retained<AVAssetReaderTrackOutput>)> {
    let path = path.to_str().context("the motion file path is not utf-8")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    unsafe {
        let asset = AVURLAsset::URLAssetWithURL_options(&url, None);
        let video = AVMediaTypeVideo.context("avfoundation names no video media type")?;
        #[allow(deprecated)]
        let tracks = asset.tracksWithMediaType(video);
        let track = tracks
            .firstObject()
            .context("the file has no video track")?;

        let format = NSNumber::new_u32(match planes {
            true => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            false => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        });
        let side = NSNumber::new_u32(edge);
        let metal = NSNumber::new_bool(true);
        let mut keys = vec![
            ns(kCVPixelBufferPixelFormatTypeKey),
            ns(kCVPixelBufferMetalCompatibilityKey),
        ];
        let mut values: Vec<&AnyObject> = vec![&format, &metal];
        if edge > 0 {
            keys.extend([ns(kCVPixelBufferWidthKey), ns(kCVPixelBufferHeightKey)]);
            let side: &AnyObject = &side;
            values.extend([side, side]);
        }
        let settings = NSDictionary::from_slices(&keys, &values);

        let output = AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            &track,
            Some(&settings),
        );
        output.setAlwaysCopiesSampleData(false);
        let reader = AVAssetReader::assetReaderWithAsset_error(&asset)
            .map_err(|error| anyhow::anyhow!("{}", error.localizedDescription()))
            .context("cannot open an asset reader")?;
        if !reader.canAddOutput(&output) {
            bail!("the asset reader refuses a video output");
        }
        reader.addOutput(&output);
        if !reader.startReading() {
            bail!("the asset reader cannot start");
        }
        Ok((reader, output))
    }
}

/// Copies a bi-planar video range buffer out into tightly packed NV12 planes, dropping any
/// padding CoreVideo added to the rows.
fn nv12(image: &CVImageBuffer) -> Result<Picture> {
    let flags = CVPixelBufferLockFlags::ReadOnly;
    if unsafe { CVPixelBufferLockBaseAddress(image, flags) } != 0 {
        bail!("cannot lock a decoded frame");
    }
    let copy = |plane: usize, bytes_per_pixel: usize| -> Option<(Vec<u8>, usize, usize)> {
        let width = CVPixelBufferGetWidthOfPlane(image, plane);
        let height = CVPixelBufferGetHeightOfPlane(image, plane);
        let stride = CVPixelBufferGetBytesPerRowOfPlane(image, plane);
        let base = CVPixelBufferGetBaseAddressOfPlane(image, plane).cast::<u8>();
        let row = width * bytes_per_pixel;
        if base.is_null() || stride < row {
            return None;
        }
        let mut packed = Vec::with_capacity(row * height);
        for line in 0..height {
            // SAFETY: the buffer is locked, and each of the plane's rows is `stride` bytes long.
            let start = unsafe { base.add(line * stride) };
            packed.extend_from_slice(unsafe { std::slice::from_raw_parts(start, row) });
        }
        Some((packed, width, height))
    };
    let planes = copy(0, 1).zip(copy(1, 2));
    unsafe { CVPixelBufferUnlockBaseAddress(image, flags) };
    let Some(((y, width, height), (cb_cr, _, _))) = planes else {
        bail!("a decoded frame has no readable planes");
    };
    Ok(Picture::Nv12 {
        width: width as u32,
        height: height as u32,
        video_range: true,
        y,
        cb_cr,
    })
}

/// A CoreVideo key as the NSString it is toll-free bridged to.
fn ns(key: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged; the key is a static constant.
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}
