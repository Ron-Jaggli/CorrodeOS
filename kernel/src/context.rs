//! What was last right-clicked, for the menu program to ask: the desktop,
//! or a dock icon and which program it starts.
//!
//! Four words, as the window server answers them: the kind (0 the
//! desktop, 1 a dock icon), the launcher's number for the program, whether
//! it is running, and its window title, 8 bytes.

use crate::arch::x86_64::without_interrupts;
use crate::sync::SpinLock;

pub const DESKTOP: u64 = 0;
pub const DOCK: u64 = 1;

static CONTEXT: SpinLock<[u64; 4]> = SpinLock::new([0; 4]);

pub fn set(words: [u64; 4]) {
    without_interrupts(|| *CONTEXT.lock() = words);
}

pub fn get() -> [u64; 4] {
    without_interrupts(|| *CONTEXT.lock())
}
