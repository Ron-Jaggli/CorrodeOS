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

/// Read the theme from the boot medium's files, if they have one.
pub fn load() {
    let text = crate::modules::find("root.tar")
        .and_then(|module| corrode_theme::find_in_tar(module.bytes, "etc/desktop.conf"))
        .and_then(|bytes| core::str::from_utf8(bytes).ok());
    match text {
        Some(text) => {
            set(Theme::parse(text));
            crate::println!("theme      : read /etc/desktop.conf");
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
