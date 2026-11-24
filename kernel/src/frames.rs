//! How long frames take: the painter's command list to the renderer, and
//! the finished frame to the screen. Summed over a few seconds of frames
//! and printed, so a test can read the cost of what it did.
//!
//! The software renderer has no clock of its own, so this times it from
//! outside, around `submit` and `present`. Glass is counted as well --
//! commands and pixels -- since it is the costly part of a frame, and
//! comparing runs with it on and off says how costly.

use corrode_render::{Command, CommandList};

use crate::sync::SpinLock;
use crate::time::now_ns;

/// How often a summary is printed, while frames are being drawn.
const WINDOW_NS: u64 = 2_000_000_000;

#[derive(Clone, Copy)]
struct Stats {
    /// When this window of frames began; zero before the first.
    since: u64,
    frames: u64,
    submit: (u64, u64),
    present: (u64, u64),
    commands: u64,
    glass: u64,
    glass_pixels: u64,
}

impl Stats {
    const NEW: Stats = Stats {
        since: 0,
        frames: 0,
        submit: (0, 0),
        present: (0, 0),
        commands: 0,
        glass: 0,
        glass_pixels: 0,
    };
}

static STATS: SpinLock<Stats> = SpinLock::new(Stats::NEW);

/// The painter's lock is the only one taken here, and both calls come
/// from the painter's task, so no interrupt handler can be holding it.
fn with<T>(f: impl FnOnce(&mut Stats) -> T) -> T {
    crate::arch::x86_64::without_interrupts(|| f(&mut STATS.lock()))
}

/// A command list took `ns` to draw.
pub fn submitted(list: &CommandList<'_>, ns: u64) {
    let (glass, pixels) = list
        .commands()
        .iter()
        .filter_map(|command| match command {
            Command::Glass { rect, .. } => Some(rect.width as u64 * rect.height as u64),
            _ => None,
        })
        .fold((0, 0), |(count, area), pixels| (count + 1, area + pixels));
    with(|stats| {
        stats.submit.0 += ns;
        stats.submit.1 = stats.submit.1.max(ns);
        stats.commands += list.commands().len() as u64;
        stats.glass += glass;
        stats.glass_pixels += pixels;
    });
}

/// A frame took `ns` to reach the screen. Ends the frame, and every
/// `WINDOW_NS` says how the last ones went.
pub fn presented(ns: u64) {
    let now = now_ns();
    let done = with(|stats| {
        if stats.since == 0 {
            stats.since = now;
        }
        stats.frames += 1;
        stats.present.0 += ns;
        stats.present.1 = stats.present.1.max(ns);
        if now - stats.since < WINDOW_NS {
            return None;
        }
        let done = *stats;
        *stats = Stats::NEW;
        Some((done, now))
    });
    let Some((stats, now)) = done else {
        return;
    };
    let frames = stats.frames.max(1);
    let elapsed_ms = (now - stats.since) / 1_000_000;
    crate::println!(
        "paint      : {} frames in {} ms; draw avg {} us max {} us; present avg {} us max {} us; {} commands, {} glass ({} kpx) a frame",
        stats.frames,
        elapsed_ms,
        stats.submit.0 / frames / 1000,
        stats.submit.1 / 1000,
        stats.present.0 / frames / 1000,
        stats.present.1 / 1000,
        stats.commands / frames,
        stats.glass / frames,
        stats.glass_pixels / frames / 1000,
    );
}
