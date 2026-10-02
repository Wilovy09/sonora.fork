//! Decodes Apple Music motion artwork, a short silent H.264 loop, into frames a view can paint.
//!
//! Each platform uses its own decoder, so nothing here ships a codec. A caller opens a local
//! file with [`open`] and drains the returned [`Frames`] at the pace of each frame's
//! presentation time; the decoder thread blocks while the queue is full and starts over at the
//! end of the file, so the loop never ends until [`Frames`] is dropped.
//!
//! On macOS a frame is the decoder's own GPU buffer, which gpui draws without a copy. Everywhere
//! else gpui has no such primitive, so a frame is plain BGRA a view wraps in an image.
//! `SONORA_MOTION_IMAGES=1` makes macOS take that path too, to try it without another machine.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use futures::channel::mpsc;

#[cfg(target_os = "macos")]
mod apple;

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
    /// Tightly packed BGRA rows, the layout gpui keeps its images in.
    Bgra {
        pixels: Vec<u8>,
        width: u32,
        height: u32,
    },
}

// SAFETY: a CVPixelBuffer is a reference-counted CoreFoundation object; retaining and releasing
// it from another thread is allowed, and the decoder never touches a buffer after sending it.
#[cfg(target_os = "macos")]
unsafe impl Send for Frame {}

/// The frames of one looping file. Dropping it stops the decoder thread.
pub type Frames = mpsc::Receiver<Frame>;

/// Whether this build can decode motion artwork at all. When it cannot, callers keep the
/// static cover and should hide anything that offers motion artwork.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Starts decoding `path` in a loop, scaled to `edge` physical pixels a side, or at its own
/// size when `edge` is zero.
pub fn open(path: &Path, edge: u32) -> Result<Frames> {
    let (sender, frames) = mpsc::channel(QUEUE);
    #[cfg(target_os = "macos")]
    apple::spawn(path, edge, sender)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, edge, sender);
        anyhow::bail!("motion artwork is not supported on this platform");
    }
    #[cfg_attr(not(target_os = "macos"), allow(unreachable_code))]
    Ok(frames)
}

/// Whether frames should come as BGRA images even where surfaces exist.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn images() -> bool {
    std::env::var("SONORA_MOTION_IMAGES").as_deref() == Ok("1")
}
