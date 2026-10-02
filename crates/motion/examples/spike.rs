//! Plays a motion artwork file in a bare window, to measure decode and paint cost.
//!
//! `cargo run -p motion --example spike -- <file.mp4> [edge]`. With `SONORA_MOTION_IMAGES=1` it
//! paints BGRA images, the path Windows and Linux take, instead of macOS surfaces.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, ObjectFit, RenderImage, Task, Window, WindowBounds, WindowOptions, div,
    img, px, rgb, size, surface,
};
use motion::Picture;

const SIDE: f32 = 768.;

/// The frame on screen, in whichever form the decoder handed it over.
enum Shown {
    Surface(motion::SurfaceBuffer),
    Image(Arc<RenderImage>),
}

struct Spike {
    frame: Option<Shown>,
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
                    Picture::Surface(buffer) => Shown::Surface(buffer),
                    Picture::Bgra {
                        pixels,
                        width,
                        height,
                    } => {
                        let image = image::RgbaImage::from_raw(width, height, pixels)
                            .expect("a frame of the size it says");
                        Shown::Image(Arc::new(RenderImage::new(vec![image::Frame::new(image)])))
                    }
                };
                let updated = this.update(cx, |this, cx| {
                    if let Some(Shown::Image(old)) = this.frame.replace(next) {
                        cx.drop_image(old, None);
                    }
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
            .map(|this| match &self.frame {
                Some(Shown::Surface(buffer)) => this.child(
                    surface(buffer.clone())
                        .object_fit(ObjectFit::Cover)
                        .size(px(SIDE)),
                ),
                Some(Shown::Image(image)) => this.child(
                    img(image.clone())
                        .object_fit(ObjectFit::Cover)
                        .size(px(SIDE)),
                ),
                None => this,
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
