//! The fullscreen cover's motion artwork: decodes the loop `state::Motion` found and paints its
//! frames over the static art, paced by each frame's own timestamp.
//!
//! It only pulls frames while fullscreen is up and the track is playing. Otherwise it holds the
//! last frame and stops reading, which leaves the decoder thread blocked on a full queue and the
//! loop where it was.
//!
//! The loop never cuts in or out. Its first frame fades in over the static art, and when the loop
//! goes away (another album, the setting turned off) its last frame stays and fades out over
//! whatever replaces it, the way a lyrics line departs while the next arrives. With motion
//! reduced both happen at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, Context, Entity, EventEmitter, ObjectFit, Pixels,
    RenderImage, Task, Window, div, img,
};
use state::{Playback, PlaybackState, Sonora};
use ui::{ActiveTheme as _, Motion, ease_in_out_cubic};

/// How often a held loop checks whether it may run again.
const HOLD: Duration = Duration::from_millis(120);

/// How late a frame may be before the clock is reset rather than chased.
const LATE: Duration = Duration::from_millis(250);

/// Says the loop started or finished showing, so the cover layer is added or dropped.
pub(crate) struct Ready;

/// A frame as the view holds it: the decoder's GPU buffer where gpui can draw one, or an image
/// built from BGRA everywhere else.
enum Shown {
    #[cfg(target_os = "macos")]
    Surface(motion::SurfaceBuffer),
    Image(Arc<RenderImage>),
}

/// Plays the motion artwork of the current track, if there is one.
pub(crate) struct MotionCover {
    motion: Entity<state::Motion>,
    playback: Entity<Playback>,
    /// The `state::Motion` revision the decoder was opened for.
    revision: Option<u64>,
    frame: Option<Shown>,
    /// Bumps with every new loop that reached its first frame, to key the fade-in.
    shown: usize,
    /// The last frame of a loop that went away, held while it fades out.
    leaving: Option<Shown>,
    /// Bumps with every loop that went away, to key the fade-out.
    departure: usize,
    task: Option<Task<()>>,
    leave: Option<Task<()>>,
}

impl EventEmitter<Ready> for MotionCover {}

impl MotionCover {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let motion = Sonora::global(cx).motion.clone();
        let playback = Sonora::global(cx).playback.clone();
        cx.observe(&motion, |this, _, cx| this.sync(cx)).detach();
        let mut this = Self {
            motion,
            playback,
            revision: None,
            frame: None,
            shown: 0,
            leaving: None,
            departure: 0,
            task: None,
            leave: None,
        };
        this.sync(cx);
        this
    }

    /// Whether anything is on screen: a playing loop, or one still fading out.
    pub(crate) fn visible(&self) -> bool {
        self.frame.is_some() || self.leaving.is_some()
    }

    /// Opens a decoder for the file `state::Motion` points at now, when it changed.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let motion = self.motion.read(cx);
        let revision = motion.revision();
        if self.revision == Some(revision) {
            return;
        }
        self.revision = Some(revision);
        let file = motion.file().map(ToOwned::to_owned);
        self.task = None;
        if let Some(frame) = self.frame.take() {
            self.depart(frame, cx);
        }
        let Some(file) = file else {
            return;
        };
        let frames = match motion::open(&file, 0) {
            Ok(frames) => frames,
            Err(error) => {
                log::warn!("motion: cannot open {}: {error:#}", file.display());
                return;
            }
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut frames = frames;
            let mut clock: Option<(Instant, Duration)> = None;
            let mut last: Option<Duration> = None;
            while let Some(frame) = frames.next().await {
                loop {
                    let Ok(live) = this.update(cx, |this, cx| this.live(cx)) else {
                        return;
                    };
                    if live {
                        break;
                    }
                    clock = None;
                    cx.background_executor().timer(HOLD).await;
                }
                // The file starting over brings the timestamps back to zero.
                if last.is_some_and(|last| frame.pts <= last) {
                    clock = None;
                }
                last = Some(frame.pts);
                let (start, base) = *clock.get_or_insert((Instant::now(), frame.pts));
                let due = start + frame.pts.saturating_sub(base);
                let now = Instant::now();
                match due.checked_duration_since(now) {
                    Some(wait) => cx.background_executor().timer(wait).await,
                    None if now - due > LATE => clock = Some((now, frame.pts)),
                    None => {}
                }
                let picture = Shown::new(frame.picture);
                let shown = this.update(cx, |this, cx| {
                    let first = this.frame.is_none();
                    if let Some(old) = this.frame.replace(picture) {
                        old.release(cx);
                    }
                    if first {
                        this.shown += 1;
                        cx.emit(Ready);
                    }
                    cx.notify();
                });
                if shown.is_err() {
                    return;
                }
            }
        }));
    }

    /// Holds the last frame of a loop that went away until it has faded out.
    fn depart(&mut self, frame: Shown, cx: &mut Context<Self>) {
        self.leave = None;
        if let Some(old) = self.leaving.take() {
            old.release(cx);
        }
        if !ui::motion::animates(cx) {
            frame.release(cx);
        } else {
            self.leaving = Some(frame);
            self.departure += 1;
            self.leave = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Motion::Slow.span()).await;
                this.update(cx, |this, cx| {
                    if let Some(old) = this.leaving.take() {
                        old.release(cx);
                    }
                    this.leave = None;
                    cx.emit(Ready);
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.emit(Ready);
        cx.notify();
    }

    /// Whether the loop may advance: fullscreen is up and the track is playing.
    fn live(&self, cx: &Context<Self>) -> bool {
        self.motion.read(cx).fullscreen()
            && matches!(self.playback.read(cx).state(), PlaybackState::Playing)
    }
}

impl Render for MotionCover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Fullscreen rounds its cover by twice the theme radius; the loop lies exactly over it.
        let radius = cx.theme().radius * 2.;
        let animates = ui::motion::animates(cx);
        let fade = || Animation::new(Motion::Slow.span()).with_easing(ease_in_out_cubic);

        div()
            .relative()
            .size_full()
            .when_some(self.leaving.as_ref(), |this, frame| {
                this.child(layer(frame, radius).with_animation(
                    ("motion-leaving", self.departure),
                    fade(),
                    |layer, t| layer.opacity(1. - t),
                ))
            })
            .when_some(self.frame.as_ref(), |this, frame| {
                this.child(match animates {
                    true => layer(frame, radius)
                        .with_animation(("motion-arriving", self.shown), fade(), |layer, t| {
                            layer.opacity(t)
                        })
                        .into_any_element(),
                    false => layer(frame, radius).into_any_element(),
                })
            })
    }
}

impl Shown {
    /// Wraps a decoded picture. BGRA becomes an image without a copy: gpui keeps images in BGRA.
    fn new(picture: motion::Picture) -> Self {
        match picture {
            #[cfg(target_os = "macos")]
            motion::Picture::Surface(buffer) => Self::Surface(buffer),
            motion::Picture::Bgra {
                pixels,
                width,
                height,
            } => {
                let frames = image::RgbaImage::from_raw(width, height, pixels)
                    .map(image::Frame::new)
                    .into_iter()
                    .collect::<Vec<_>>();
                Self::Image(Arc::new(RenderImage::new(frames)))
            }
        }
    }

    /// Lets go of the frame. An image's texture is freed at once, since a loop paints dozens
    /// of them a second and none comes back.
    fn release(self, cx: &mut App) {
        match self {
            #[cfg(target_os = "macos")]
            Self::Surface(_) => {}
            Self::Image(image) => cx.drop_image(image, None),
        }
    }
}

/// One frame filling the cover, rounded like it. The opacity of the layer reaches the frame.
fn layer(frame: &Shown, radius: Pixels) -> gpui::Div {
    let layer = div().absolute().inset_0();
    match frame {
        #[cfg(target_os = "macos")]
        Shown::Surface(buffer) => layer.child(
            gpui::surface(buffer.clone())
                .object_fit(ObjectFit::Cover)
                .size_full()
                .rounded(radius),
        ),
        Shown::Image(image) => layer.child(
            img(image.clone())
                .object_fit(ObjectFit::Cover)
                .size_full()
                .rounded(radius),
        ),
    }
}
