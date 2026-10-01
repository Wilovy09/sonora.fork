//! The fullscreen cover's motion artwork: decodes the loop `state::Motion` found and paints its
//! frames over the static art, paced by each frame's own timestamp.
//!
//! It only pulls frames while fullscreen is up and the track is playing. Otherwise it holds the
//! last frame and stops reading, which leaves the decoder thread blocked on a full queue and the
//! loop where it was.

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{Context, Entity, EventEmitter, Task, Window, div};
use state::{Playback, PlaybackState, Sonora};

/// How often a held loop checks whether it may run again.
const HOLD: Duration = Duration::from_millis(120);

/// How late a frame may be before the clock is reset rather than chased.
const LATE: Duration = Duration::from_millis(250);

/// Says the loop has its first frame of a new file, or lost it, so the cover layer can fade.
pub(crate) struct Ready;

/// Plays the motion artwork of the current track, if there is one.
pub(crate) struct MotionCover {
    motion: Entity<state::Motion>,
    playback: Entity<Playback>,
    /// The `state::Motion` revision the decoder was opened for.
    revision: Option<u64>,
    frame: Option<motion::Frame>,
    /// Bumps with every new loop that reached its first frame, to key the fade-in.
    shown: usize,
    task: Option<Task<()>>,
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
            task: None,
        };
        this.sync(cx);
        this
    }

    /// Whether a frame is ready to cover the static art.
    pub(crate) fn ready(&self) -> bool {
        self.frame.is_some()
    }

    /// Changes with every loop that became ready, for the layer's fade-in key.
    pub(crate) fn shown(&self) -> usize {
        self.shown
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
        if self.frame.take().is_some() {
            cx.emit(Ready);
            cx.notify();
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
                let shown = this.update(cx, |this, cx| {
                    let first = this.frame.is_none();
                    this.frame = Some(frame);
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

    /// Whether the loop may advance: fullscreen is up and the track is playing.
    fn live(&self, cx: &Context<Self>) -> bool {
        self.motion.read(cx).fullscreen()
            && matches!(self.playback.read(cx).state(), PlaybackState::Playing)
    }
}

impl Render for MotionCover {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().map(|this| {
            #[cfg(target_os = "macos")]
            let this = this.when_some(self.frame.as_ref(), |this, frame| {
                this.child(
                    gpui::surface(frame.buffer.clone())
                        .object_fit(gpui::ObjectFit::Cover)
                        .size_full(),
                )
            });
            this
        })
    }
}
