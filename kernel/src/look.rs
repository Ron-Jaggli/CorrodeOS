//! The desktop's look: read once from `/etc/desktop.conf` in the files the
//! ISO carries, and changed while running by whoever is allowed to.
//!
//! The parsing is `corrode-theme`'s, tested on the build host. What is kept
//! here is the one current theme, and a count of how many times it has
//! changed, so the painter can notice a change with one load and no lock.

use core::sync::atomic::{AtomicU32, Ordering};

use corrode_theme::Theme;

use crate::arch::x86_64::without_interrupts;
use crate::sync::SpinLock;

static CURRENT: SpinLock<Theme> = SpinLock::new(corrode_theme::CORRODE);
static CHANGES: AtomicU32 = AtomicU32::new(0);

/// The lock screen's passcode, if desktop.conf sets one. Only this file
/// sees it: it is not part of the theme programs are told.
static PASSCODE: SpinLock<Option<[u8; 8]>> = SpinLock::new(None);
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
            let code = corrode_theme::passcode(text);
            without_interrupts(|| *PASSCODE.lock() = code);
            crate::println!(
                "theme      : read /etc/desktop.conf{}",
                if code.is_some() { ", with a lock screen passcode" } else { "" }
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
    without_interrupts(|| {
        let mut tries = TRIES.lock();
        let (wrong, next) = *tries;
        if now < next {
            return Err((next - now).div_ceil(1_000_000_000));
        }
        // Every byte looked at, whatever the first difference: how long the
        // answer takes says nothing about how much of it was right.
        let right = passcode.iter().zip(code).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0;
        *tries = if right {
            (0, 0)
        } else if wrong + 1 >= FREE_TRIES {
            (0, now + WAIT_NS)
        } else {
            (wrong + 1, 0)
        };
        Ok(right)
    })
}
