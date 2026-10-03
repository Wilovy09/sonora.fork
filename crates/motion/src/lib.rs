//! Decodes Apple Music motion artwork, a short silent H.264 loop, into frames a view can paint.
//!
//! Each platform uses its own decoder, so nothing here ships a codec. A caller opens a local
//! file with [`open`] and drains the returned [`Frames`] at the pace of each frame's
//! presentation time; the decoder thread blocks while the queue is full and starts over at the
//! end of the file, so the loop never ends until [`Frames`] is dropped.
//!
//! The decoders are AVFoundation on macOS, Media Foundation on Windows and GStreamer on Linux,
//! which is opened at runtime rather than linked. On macOS a frame is the decoder's
//! own GPU buffer, which gpui draws without a copy. Everywhere else a frame is NV12 planes in
//! memory, the layout those decoders produce and gpui's surfaces take on every renderer.
//! `SONORA_MOTION_NV12=1` makes macOS hand out planes too, to try that path without another
//! machine.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use futures::channel::mpsc;

#[cfg(target_os = "macos")]
mod apple;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod gst;
#[cfg(windows)]
mod media_foundation;

/// How many decoded frames may wait for the view. Small, because a frame is a GPU-ready buffer
/// and the queue only has to cover one late repaint.
const QUEUE: usize = 3;

/// The GPU buffer gpui's `surface` element draws on macOS.
#[cfg(target_os = "macos")]
pub use core_video::pixel_buffer::CVPixelBuffer as SurfaceBuffer;

/// One decoded picture and when it should be on screen, measured from the start of the loop.
pub struct Frame {
    pub picture: Picture,
    pub pts: Duration,
}

/// What a frame holds, depending on how the platform can draw it.
pub enum Picture {
    /// A bi-planar 4:2:0 buffer for gpui's `surface` element.
    #[cfg(target_os = "macos")]
    Surface(SurfaceBuffer),
    /// NV12 in memory: a full size luma plane, then a half size plane of interleaved Cb and Cr,
    /// rows tightly packed. `video_range` says black is 16 and white 235 rather than 0 and 255.
    Nv12 {
        width: u32,
        height: u32,
        video_range: bool,
        y: Vec<u8>,
        cb_cr: Vec<u8>,
    },
}

// SAFETY: a CVPixelBuffer is a reference-counted CoreFoundation object; retaining and releasing
// it from another thread is allowed, and the decoder never touches a buffer after sending it.
#[cfg(target_os = "macos")]
unsafe impl Send for Frame {}

/// The frames of one looping file. Dropping it stops the decoder thread.
pub type Frames = mpsc::Receiver<Frame>;

/// Whether this build and system can decode motion artwork at all. When they cannot, callers
/// keep the static cover and should hide anything that offers motion artwork. On Linux this
/// asks GStreamer for an H.264 decoder, so it can be false on a machine without one.
pub fn supported() -> bool {
    #[cfg(any(target_os = "macos", windows))]
    return true;
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    return gst::supported();
    #[allow(unreachable_code)]
    false
}

/// Starts decoding `path` in a loop. On macOS frames are scaled to `edge` physical pixels a
/// side, or kept at their own size when `edge` is zero; elsewhere they keep their own size.
pub fn open(path: &Path, edge: u32) -> Result<Frames> {
    let (sender, frames) = mpsc::channel(QUEUE);
    #[cfg(target_os = "macos")]
    apple::spawn(path, edge, sender)?;
    #[cfg(windows)]
    media_foundation::spawn(path, edge, sender)?;
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    gst::spawn(path, edge, sender)?;
    #[cfg(not(any(
        target_os = "macos",
        windows,
        any(target_os = "linux", target_os = "freebsd")
    )))]
    {
        let _ = (path, edge, sender, frames);
        anyhow::bail!("motion artwork is not supported on this platform");
    }
    #[allow(unreachable_code)]
    Ok(frames)
}

/// Whether frames should come as NV12 planes even where GPU buffers exist.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn planes() -> bool {
    std::env::var("SONORA_MOTION_NV12").as_deref() == Ok("1")
}
