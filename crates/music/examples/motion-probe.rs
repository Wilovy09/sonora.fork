//! Looks a track's motion artwork up in the Apple Music catalog and downloads the loop.
//!
//! Run with `cargo run --example motion-probe --package music -- "<title>" "<artist>"
//! ["<album>"] [seconds] [edge] [out.mp4]`.
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use music::motion::{MotionQuery, MotionSearch};

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let mut args = std::env::args().skip(1);
    let title = args.next().context("no title given")?;
    let artist = args.next().context("no artist given")?;
    let album = args.next();
    let duration = args
        .next()
        .and_then(|it| it.parse().ok())
        .map(Duration::from_secs);
    let edge = args.next().and_then(|it| it.parse().ok()).unwrap_or(768);
    let out = args.next();

    let search = MotionSearch::default();
    let query = MotionQuery::new(&title, &artist, album.as_deref(), duration);
    println!("key: {}", query.key());

    let started = Instant::now();
    let Some(art) = search.find(&query).await? else {
        println!("no motion artwork ({:?})", started.elapsed());
        return Ok(());
    };
    println!(
        "album {}: {} ({:?})",
        art.album_id,
        art.master,
        started.elapsed()
    );

    let started = Instant::now();
    let found = search.fetch(&art, edge).await?;
    println!(
        "loop: {} px, {} bytes ({:?})",
        found.side,
        found.bytes.len(),
        started.elapsed()
    );
    if let Some(out) = out {
        std::fs::write(&out, &found.bytes).with_context(|| format!("cannot write {out}"))?;
        println!("wrote {out}");
    }
    Ok(())
}
