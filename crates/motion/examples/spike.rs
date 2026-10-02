//! Plays a motion artwork file in a bare window, to measure decode and paint cost.
//!
//! `cargo run -p motion --example spike -- <file.mp4> [edge]`. With `SONORA_MOTION_NV12=1` it
//! paints NV12 planes from memory, the path Windows and Linux take, instead of macOS buffers.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Nv12Frame, Nv12Range, ObjectFit, SurfaceSource, Task, Window,
    WindowBounds, WindowOptions, div, px, rgb, size, surface,
};
use motion::Picture;

const SIDE: f32 = 768.;

struct Spike {
    frame: Option<SurfaceSource>,
    _task: Task<()>,
}

impl Spike {
    fn new(path: PathBuf, edge: u32, cx: &mut Context<Self>) -> Self {
        let mut frames = motion::open(&path, edge).expect("cannot open the motion file");
        let task = cx.spawn(async move |this, cx| {
            let mut shown = 0u64;
            let mut window = Instant::now();
            let mut last: Option<Duration> = None;
            while let Some(frame) = frames.next().await {
                let wait = match last {
                    Some(last) if frame.pts > last => frame.pts - last,
                    _ => Duration::from_millis(33),
                };
                last = Some(frame.pts);
                cx.background_executor()
                    .timer(wait.min(Duration::from_millis(100)))
                    .await;
                let next = match frame.picture {
                    #[cfg(target_os = "macos")]
                    Picture::Surface(buffer) => SurfaceSource::Surface(buffer),
                    Picture::Nv12 {
                        width,
                        height,
                        video_range,
                        y,
                        cb_cr,
                    } => {
                        let range = match video_range {
                            true => Nv12Range::Video,
                            false => Nv12Range::Full,
                        };
                        SurfaceSource::Nv12(
                            Nv12Frame::new(width, height, range, y, cb_cr)
                                .expect("a frame of the size it says"),
                        )
                    }
                };
                let updated = this.update(cx, |this, cx| {
                    this.frame = Some(next);
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
                shown += 1;
                if window.elapsed() >= Duration::from_secs(2) {
                    eprintln!(
                        "spike: {:.1} fps",
                        shown as f64 / window.elapsed().as_secs_f64()
                    );
                    shown = 0;
                    window = Instant::now();
                }
            }
        });
        Self {
            frame: None,
            _task: task,
        }
    }
}

impl Render for Spike {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x101010))
            .flex()
            .items_center()
            .justify_center()
            .when_some(self.frame.clone(), |this, frame| {
                this.child(
                    surface(frame)
                        .object_fit(ObjectFit::Cover)
                        .size(px(SIDE))
                        .rounded(px(48.)),
                )
            })
    }
}

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: spike <file.mp4> [edge]"));
    let edge = args.next().and_then(|it| it.parse().ok()).unwrap_or(768);
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(SIDE + 64.), px(SIDE + 64.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| Spike::new(path, edge, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
