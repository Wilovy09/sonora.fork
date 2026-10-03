//! The Windows decoder: a Media Foundation source reader that hands back NV12, which is what the
//! system H.264 decoder produces anyway, copied out of the decoder's padded frames into tightly
//! packed planes.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::SinkExt as _;
use futures::channel::mpsc;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFMediaType_Video, MFNominalRange_0_255, MFSTARTUP_LITE, MFShutdown, MFStartup, MFVideoArea,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING, Interface as _};

use crate::{Frame, Picture};

/// The reader's first video stream, as the index its methods take.
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Where in a decoded frame the picture lies. Decoders pad frames to whole macroblocks, so the
/// buffer can be taller and wider than what is shown.
struct Geometry {
    /// Rows in the padded luma plane, which is where the chroma plane starts.
    coded_height: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    video_range: bool,
}

/// Opens `path` on a thread of its own, reports there whether the system can read it, then
/// decodes it forever. Media Foundation scales nothing here: the GPU draws the frame at any size,
/// so `edge` is not needed.
pub(crate) fn spawn(path: &Path, _edge: u32, sender: mpsc::Sender<Frame>) -> Result<()> {
    let path = path.to_path_buf();
    let (opened, ready) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("motion-decoder".into())
        .spawn(move || {
            let _session = match Session::start() {
                Ok(session) => session,
                Err(error) => {
                    opened.send(Err(error)).ok();
                    return;
                }
            };
            let reader = match reader(&path) {
                Ok(reader) => {
                    opened.send(Ok(())).ok();
                    reader
                }
                Err(error) => {
                    opened.send(Err(error)).ok();
                    return;
                }
            };
            if let Err(error) = decode(&reader, sender) {
                log::warn!("motion: cannot decode {}: {error:#}", path.display());
            }
        })
        .context("cannot start the motion decoder thread")?;
    ready
        .recv()
        .context("the motion decoder thread stopped before opening the file")?
}

/// COM and Media Foundation for the life of one decoder thread.
struct Session;

impl Session {
    fn start() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("cannot initialise com")?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
                CoUninitialize();
                return Err(error).context("cannot start media foundation");
            }
        }
        Ok(Self)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            MFShutdown().ok();
            CoUninitialize();
        }
    }
}

/// A source reader over the file, set to hand out NV12.
fn reader(path: &Path) -> Result<IMFSourceReader> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), None)
            .context("cannot open the file with media foundation")?;
        let wanted = MFCreateMediaType().context("cannot create a media type")?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        reader
            .SetCurrentMediaType(STREAM, None, &wanted)
            .context("media foundation cannot decode the file to nv12")?;
        Ok(reader)
    }
}

/// Reads frames until the receiver goes away, going back to the start at the end of the file.
fn decode(reader: &IMFSourceReader, mut sender: mpsc::Sender<Frame>) -> Result<()> {
    let mut geometry = geometry(reader)?;
    let mut sent_this_pass = 0usize;
    loop {
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            reader
                .ReadSample(
                    STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .context("cannot read a frame")?;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            geometry = self::geometry(reader)?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            if sent_this_pass == 0 {
                bail!("the file has no video frames");
            }
            rewind(reader)?;
            sent_this_pass = 0;
            continue;
        }
        let Some(sample) = sample else {
            continue;
        };
        let picture = copy(&sample, &geometry)?;
        // Media Foundation counts time in hundreds of nanoseconds.
        let pts = Duration::from_nanos(timestamp.max(0) as u64 * 100);
        if futures::executor::block_on(sender.send(Frame { picture, pts })).is_err() {
            return Ok(());
        }
        sent_this_pass += 1;
    }
}

/// Seeks back to the first frame.
fn rewind(reader: &IMFSourceReader) -> Result<()> {
    let mut position = PROPVARIANT::default();
    unsafe {
        let value = &mut position.Anonymous.Anonymous;
        value.vt = VT_I8;
        value.Anonymous.hVal = 0;
        reader
            .SetCurrentPosition(&GUID::zeroed(), &position)
            .context("cannot go back to the start of the file")
    }
}

/// The size, crop and range of the frames the reader hands out now.
fn geometry(reader: &IMFSourceReader) -> Result<Geometry> {
    unsafe {
        let current: IMFMediaType = reader
            .GetCurrentMediaType(STREAM)
            .context("cannot read the decoded format")?;
        let size = current
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .context("the decoded format has no frame size")?;
        let (coded_width, coded_height) = ((size >> 32) as u32, size as u32);
        let mut aperture = MFVideoArea::default();
        let shown = current
            .GetBlob(
                &MF_MT_MINIMUM_DISPLAY_APERTURE,
                std::slice::from_raw_parts_mut(
                    (&mut aperture as *mut MFVideoArea).cast::<u8>(),
                    std::mem::size_of::<MFVideoArea>(),
                ),
                None,
            )
            .is_ok();
        let (left, top, width, height) = match shown {
            true => (
                aperture.OffsetX.value.max(0) as u32,
                aperture.OffsetY.value.max(0) as u32,
                aperture.Area.cx.max(0) as u32,
                aperture.Area.cy.max(0) as u32,
            ),
            false => (0, 0, coded_width, coded_height),
        };
        if width == 0 || height == 0 || left + width > coded_width || top + height > coded_height {
            bail!("the decoded frame has no usable picture");
        }
        let video_range = current
            .GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
            .map(|range| range != MFNominalRange_0_255.0 as u32)
            .unwrap_or(true);
        Ok(Geometry {
            coded_height,
            left,
            top,
            width,
            height,
            video_range,
        })
    }
}

/// Copies the visible part of an NV12 sample into tightly packed planes.
fn copy(sample: &IMFSample, geometry: &Geometry) -> Result<Picture> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .context("cannot read a decoded frame")?;
        let length = buffer
            .GetCurrentLength()
            .context("cannot size a decoded frame")? as usize;
        let planar: IMF2DBuffer = buffer
            .cast()
            .context("a decoded frame is not a 2d buffer")?;
        let mut scanline = std::ptr::null_mut();
        let mut pitch = 0i32;
        planar
            .Lock2D(&mut scanline, &mut pitch)
            .context("cannot lock a decoded frame")?;
        let copied = pack(scanline, pitch, length, geometry);
        planar.Unlock2D().ok();
        copied
    }
}

/// Packs the planes out of a locked frame whose rows are `pitch` bytes apart.
///
/// # Safety
///
/// `scanline` must point at the first of `length` readable bytes laid out as NV12.
unsafe fn pack(
    scanline: *const u8,
    pitch: i32,
    length: usize,
    geometry: &Geometry,
) -> Result<Picture> {
    let Ok(pitch) = usize::try_from(pitch) else {
        bail!("a decoded frame is stored bottom up");
    };
    let coded_height = geometry.coded_height as usize;
    if scanline.is_null() || pitch * (coded_height + coded_height.div_ceil(2)) > length {
        bail!("a decoded frame is smaller than its format says");
    }
    let (left, top) = (geometry.left as usize, geometry.top as usize);
    let (width, height) = (geometry.width as usize, geometry.height as usize);
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    if left + width > pitch || (left / 2 + chroma_width) * 2 > pitch {
        bail!("a decoded frame is wider than its rows");
    }

    let rows = |start: *const u8, first: usize, count: usize, offset: usize, bytes: usize| {
        let mut packed = Vec::with_capacity(count * bytes);
        for line in first..first + count {
            // SAFETY: every row read lies inside the `length` bytes checked above.
            let row = unsafe { start.add(line * pitch + offset) };
            packed.extend_from_slice(unsafe { std::slice::from_raw_parts(row, bytes) });
        }
        packed
    };
    let y = rows(scanline, top, height, left, width);
    // SAFETY: the chroma plane starts right after the padded luma plane.
    let chroma = unsafe { scanline.add(pitch * coded_height) };
    let cb_cr = rows(
        chroma,
        top / 2,
        chroma_height,
        (left / 2) * 2,
        chroma_width * 2,
    );
    Ok(Picture::Nv12 {
        width: geometry.width,
        height: geometry.height,
        video_range: geometry.video_range,
        y,
        cb_cr,
    })
}
