//! The desktop's look: read once from `/etc/desktop.conf` in the files the
//! ISO carries, and changed while running by whoever is allowed to.
//!
//! The parsing is `corrode-theme`'s, tested on the build host. What is kept
//! here is the one current theme, and a count of how many times it has
//! changed, so the painter can notice a change with one load and no lock.

use core::sync::atomic::{AtomicU32, Ordering};

use corrode_theme::Theme;
use corrode_theme::passcode::{self, Hashed, Stored};

use crate::arch::x86_64::without_interrupts;
use crate::sync::SpinLock;

static CURRENT: SpinLock<Theme> = SpinLock::new(corrode_theme::CORRODE);
static CHANGES: AtomicU32 = AtomicU32::new(0);

/// A salted hash of the lock screen's passcode, if desktop.conf sets one.
/// Only this file sees it: it is not part of the theme programs are told.
/// A passcode given in the clear is hashed as it is read, so the digits
/// are never kept.
static PASSCODE: SpinLock<Option<Hashed>> = SpinLock::new(None);
/// Wrong tries in a row, and when tries are next taken.
static TRIES: SpinLock<(u32, u64)> = SpinLock::new((0, 0));
/// After this many wrong tries in a row, a wait of `WAIT_NS` before the next.
const FREE_TRIES: u32 = 3;
const WAIT_NS: u64 = 30_000_000_000;

/// Read the theme from the boot medium's files, if they have one.
pub fn load() {
    let text = crate::modules::find("root.tar")
        .and_then(|module| corrode_theme::find_in_tar(module.bytes, "etc/desktop.conf"))
        .and_then(|bytes| core::str::from_utf8(bytes).ok());
    match text {
        Some(text) => {
            set(Theme::parse(text));
            let stored = corrode_theme::passcode(text);
            let hashed = stored.map(|stored| match stored {
                Stored::Hashed(hashed) => hashed,
                // The salt need not be secret, and this hash never leaves
                // memory: the clock is salt enough to make it this boot's.
                Stored::Plain(code) => {
                    let mut salt = [0u8; 16];
                    salt[..8].copy_from_slice(&crate::time::now_ns().to_le_bytes());
                    Hashed::new(code, salt, passcode::ITERATIONS)
                }
            });
            without_interrupts(|| *PASSCODE.lock() = hashed);
            crate::println!(
                "theme      : read /etc/desktop.conf{}",
                match stored {
                    Some(Stored::Hashed(_)) => ", with a lock screen passcode (hashed)",
                    Some(Stored::Plain(_)) =>
                        ", with a lock screen passcode (in the clear: ./corrode passcode-hash hashes it)",
                    None => "",
                }
            );
        }
        None => crate::println!("theme      : no /etc/desktop.conf -- the default look"),
    }
}

pub fn current() -> Theme {
    without_interrupts(|| *CURRENT.lock())
}

/// How many times the theme has changed: compare to notice one.
pub fn changes() -> u32 {
    CHANGES.load(Ordering::Acquire)
}

pub fn set(theme: Theme) {
    without_interrupts(|| *CURRENT.lock() = theme);
    CHANGES.fetch_add(1, Ordering::Release);
}

/// Whether the lock screen needs a passcode.
pub fn has_passcode() -> bool {
    without_interrupts(|| PASSCODE.lock().is_some())
}

/// Is `code` the passcode? `Some(true)` or `Some(false)`, or `None` with
/// the seconds to wait if too many wrong tries came just before. With no
/// passcode set, anything is right.
pub fn try_passcode(code: [u8; 8], now: u64) -> Result<bool, u64> {
    let Some(passcode) = without_interrupts(|| *PASSCODE.lock()) else {
        return Ok(true);
    };
    let wait = without_interrupts(|| {
        let (_, next) = *TRIES.lock();
        (now < next).then(|| (next - now).div_ceil(1_000_000_000))
    });
    if let Some(wait) = wait {
        return Err(wait);
    }
    // Slow on purpose, so with interrupts on and no lock held. Only the
    // lock screen may try, through the desktop's one loop, so no second
    // try can start in between.
    let right = passcode.matches(code);
    without_interrupts(|| {
        let mut tries = TRIES.lock();
        let (wrong, _) = *tries;
        *tries = if right {
            (0, 0)
        } else if wrong + 1 >= FREE_TRIES {
            (0, now + WAIT_NS)
        } else {
            (wrong + 1, 0)
        };
    });
    Ok(right)
}
