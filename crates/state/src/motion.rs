//! Follows the playing track to the motion artwork file fullscreen loops over its cover.
//!
//! What an album has is remembered in `cache.sqlite`, so a miss is not asked again for a week and
//! a hit never searches again. The loop itself is kept on disk under the cache directory, where
//! the decoder opens it, with the oldest files dropped past a size budget. Nothing runs while
//! the setting is off, since a lookup sends the track's names to Apple's catalog.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use gpui::{Context, Entity, Task};
use music::motion::{MotionArt, MotionQuery, MotionSearch};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{AppSettings, Io, Playback, Session, SessionEvent, join};

/// The prefix of every key this entity writes to `cache.sqlite`.
const PREFIX: &str = "motion/";

/// How long an album known to have no motion artwork is left alone.
const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The sides a view's request is rounded up to, so a window resize does not refetch. They are
/// sizes Apple encodes loops at; the last also caps what is ever asked for.
const EDGES: [u32; 3] = [486, 768, 1080];

/// How much disk the downloaded loops may take before the oldest go.
const BUDGET: u64 = 96 << 20;

/// The motion artwork of the playing track, once its file is on disk.
pub struct Motion {
    session: Entity<Session>,
    playback: Entity<Playback>,
    settings: Entity<AppSettings>,
    search: MotionSearch,
    cache: storage::Cache,
    io: Io,
    /// The track the current state was resolved for, so a repaint of playback is a no-op.
    track: Option<String>,
    key: Option<String>,
    file: Option<PathBuf>,
    revision: u64,
    /// The side asked for, or zero until fullscreen has been up once. Nothing is looked up before
    /// then, so a listener who never opens fullscreen never sends a lookup.
    edge: u32,
    fullscreen: bool,
    task: Option<Task<()>>,
}

/// What `cache.sqlite` remembers about one album.
#[derive(Serialize, Deserialize)]
struct Remembered {
    album_id: Option<String>,
    master: Option<String>,
}

impl Motion {
    pub fn new(
        session: Entity<Session>,
        playback: Entity<Playback>,
        settings: Entity<AppSettings>,
        cache: storage::Cache,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&playback, |this, _, cx| this.follow(cx))
            .detach();
        cx.observe(&settings, |this, _, cx| this.follow(cx))
            .detach();
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedOut => this.forget(cx),
            SessionEvent::SignedIn | SessionEvent::Reconnected | SessionEvent::LocalChanged => {}
        })
        .detach();

        Self {
            session,
            playback,
            settings,
            search: MotionSearch::default(),
            cache,
            io,
            track: None,
            key: None,
            file: None,
            revision: 0,
            edge: 0,
            fullscreen: false,
            task: None,
        }
    }

    /// The loop to play over the current cover, if it has one and the setting is on.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Bumps whenever [`file`](Self::file) changes, so a view knows to open a new decoder.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether fullscreen is up, which is the only place the loop plays.
    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Records whether fullscreen is up. The loop keeps its place while it is down.
    pub fn set_fullscreen(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.fullscreen == on {
            return;
        }
        self.fullscreen = on;
        cx.notify();
    }

    /// Asks for a rendition at least `edge` physical pixels on a side. Only growing it refetches,
    /// so a window shrinking never throws a good loop away.
    pub fn want_edge(&mut self, edge: u32, cx: &mut Context<Self>) {
        let edge = EDGES
            .into_iter()
            .find(|side| *side >= edge)
            .unwrap_or(EDGES[EDGES.len() - 1]);
        if edge <= self.edge {
            return;
        }
        self.edge = edge;
        self.track = None;
        self.key = None;
        self.follow(cx);
    }

    fn forget(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        self.track = None;
        self.key = None;
        self.show(None, cx);
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        if !self.settings.read(cx).motion_artwork() {
            if self.track.is_some() || self.file.is_some() {
                self.forget(cx);
            }
            return;
        }
        if self.edge == 0 {
            return;
        }
        // Playback notifies on every position tick, so the track is only cloned once it changed.
        let playback = self.playback.read(cx);
        let id = playback.track().map(|track| {
            track
                .id
                .clone()
                .unwrap_or_else(|| format!("{}|{}", track.artists, track.name))
        });
        if id.is_some() && id == self.track {
            return;
        }
        let track = playback.track().cloned();
        self.track = id;

        let apple = track
            .as_ref()
            .and_then(|track| track.id.as_deref())
            .and_then(|id| self.session.read(cx).slug_for(id))
            == Some("apple");
        let query = track
            .as_ref()
            .and_then(|track| MotionQuery::for_track(track, apple));
        let key = query.as_ref().map(MotionQuery::key);
        if key == self.key {
            return;
        }
        self.task = None;
        self.key = key.clone();
        self.show(None, cx);

        let (Some(query), Some(key)) = (query, key) else {
            return;
        };
        self.load(query, key, cx);
    }

    fn load(&mut self, query: MotionQuery, key: String, cx: &mut Context<Self>) {
        let io = self.io.clone();
        let search = self.search.clone();
        let cache = self.cache.clone();
        let edge = self.edge;
        let wanted = key.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let found = join(
                io.spawn(async move { resolve(&search, &cache, &wanted, &query, edge).await }),
            )
            .await;

            this.update(cx, |this, cx| {
                this.task = None;
                if this.key.as_deref() != Some(key.as_str()) {
                    return;
                }
                match found {
                    Ok(file) => this.show(file, cx),
                    Err(error) => log::warn!("motion: cannot load {key}: {error:#}"),
                }
            })
            .ok();
        }));
    }

    fn show(&mut self, file: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.file == file {
            return;
        }
        self.file = file;
        self.revision += 1;
        cx.notify();
    }
}

/// The loop file for `query`: remembered or searched for, then downloaded unless it already is.
async fn resolve(
    search: &MotionSearch,
    cache: &storage::Cache,
    key: &str,
    query: &MotionQuery,
    edge: u32,
) -> Result<Option<PathBuf>> {
    let Some(art) = remembered_or_found(search, cache, key, query).await? else {
        return Ok(None);
    };
    let path = file_for(&art, edge)?;
    if path.is_file() {
        return Ok(Some(path));
    }
    let found = search.fetch(&art, edge).await?;
    let parent = path.parent().context("cannot place the motion artwork")?;
    std::fs::create_dir_all(parent).context("cannot create the motion artwork cache")?;
    let partial = path.with_extension("part");
    std::fs::write(&partial, &found.bytes).context("cannot write the motion artwork")?;
    std::fs::rename(&partial, &path).context("cannot keep the motion artwork")?;
    log::info!(
        "motion: kept a {} px loop of {} bytes for {key}",
        found.side,
        found.bytes.len()
    );
    prune(parent, &path);
    Ok(Some(path))
}

/// What the cache says about the album, or what a fresh lookup finds and the cache then keeps.
/// A transient failure is never written, so it is tried again on the next play.
async fn remembered_or_found(
    search: &MotionSearch,
    cache: &storage::Cache,
    key: &str,
    query: &MotionQuery,
) -> Result<Option<MotionArt>> {
    let stored = format!("{PREFIX}{key}");
    if let Some((remembered, saved_at)) = read(cache, &stored) {
        match (remembered.album_id, remembered.master) {
            (Some(album_id), Some(master)) => return Ok(Some(MotionArt { album_id, master })),
            _ if now().saturating_sub(saved_at) < MISS_TTL.as_secs() as i64 => return Ok(None),
            _ => {}
        }
    }
    let found = search.find(query).await?;
    let remembered = Remembered {
        album_id: found.as_ref().map(|art| art.album_id.clone()),
        master: found.as_ref().map(|art| art.master.clone()),
    };
    let value = serde_json::to_string(&remembered).context("cannot encode the motion artwork")?;
    if let Err(error) = cache.write(&stored, &value, now()) {
        log::warn!("motion: cannot remember {key}: {error:#}");
    }
    Ok(found)
}

fn read(cache: &storage::Cache, key: &str) -> Option<(Remembered, i64)> {
    let connection = cache.open().ok()?;
    let (value, saved_at): (String, i64) = connection
        .query_row(
            "SELECT value, saved_at FROM snapshots WHERE key = ?",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    Some((serde_json::from_str(&value).ok()?, saved_at))
}

/// Where the loop of `art` at `edge` lives. The master url names the loop, so the file is
/// shared by every key that resolves to the same album.
fn file_for(art: &MotionArt, edge: u32) -> Result<PathBuf> {
    let digest = Sha256::digest(art.master.as_bytes());
    let name: String = digest
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let dir = dirs::cache_dir()
        .context("there is no cache directory")?
        .join("sonora")
        .join("motion");
    Ok(dir.join(format!("{name}-{edge}.mp4")))
}

/// Drops the least recently written loops until the folder fits [`BUDGET`], never `keep`.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            meta.is_file().then(|| {
                (
                    meta.modified().unwrap_or(UNIX_EPOCH),
                    meta.len(),
                    entry.path(),
                )
            })
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= BUDGET {
            break;
        }
        if path == keep {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|it| it.as_secs() as i64)
        .unwrap_or(0)
}
