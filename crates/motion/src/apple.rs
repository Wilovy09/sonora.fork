//! The macOS decoder: AVAssetReader hands back hardware-decoded, IOSurface-backed pixel buffers
//! in the bi-planar 4:2:0 layout gpui's `surface` element draws without a copy.

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
    kCVPixelBufferHeightKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use crate::Frame;

/// Opens `path` once on the caller's thread, so a file the system cannot read fails here, then
/// decodes it forever on a thread of its own.
pub(crate) fn spawn(path: &Path, edge: u32, sender: mpsc::Sender<Frame>) -> Result<()> {
    let path = path.to_path_buf();
    autoreleasepool(|_| reader(&path, edge).map(drop))?;
    std::thread::Builder::new()
        .name("motion-decoder".into())
        .spawn(move || {
            if let Err(error) = decode(&path, edge, sender) {
                log::warn!("motion: cannot decode {}: {error:#}", path.display());
            }
        })
        .context("cannot start the motion decoder thread")?;
    Ok(())
}

/// Reads the file from the start each time it runs out, until the receiver goes away.
fn decode(path: &Path, edge: u32, mut sender: mpsc::Sender<Frame>) -> Result<()> {
    loop {
        let finished = autoreleasepool(|_| -> Result<bool> {
            let (reader, output) = reader(path, edge)?;
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
                let raw =
                    objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr() as CVPixelBufferRef;
                let buffer = unsafe { CVPixelBuffer::wrap_under_get_rule(raw) };
                if futures::executor::block_on(sender.send(Frame { buffer, pts })).is_err() {
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

/// A started reader over the file's first video track, asking for buffers scaled to `edge`.
fn reader(
    path: &Path,
    edge: u32,
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

        let format = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
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

/// A CoreVideo key as the NSString it is toll-free bridged to.
fn ns(key: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged; the key is a static constant.
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}
