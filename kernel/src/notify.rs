//! Notifications: what programs have asked the desktop to tell the person.
//!
//! A program sends `window::NOTIFY` to the window server with up to 32
//! bytes of text; the server files it here under the program's window
//! title, and the painter takes them one at a time to show as banners.

use alloc::collections::VecDeque;
use alloc::string::String;

use crate::arch::x86_64::without_interrupts;
use crate::sync::SpinLock;

/// Waiting to be shown: from whom, and what. More than this many waiting
/// and the oldest goes, so a program in a loop cannot fill memory.
static PENDING: SpinLock<VecDeque<(String, String)>> = SpinLock::new(VecDeque::new());
const MOST: usize = 8;

/// File a notification from `from` saying `text`.
pub fn post(from: String, text: String) {
    crate::println!("notify     : {from}: {text}");
    without_interrupts(|| {
        let mut pending = PENDING.lock();
        if pending.len() == MOST {
            pending.pop_front();
        }
        pending.push_back((from, text));
    });
}

/// The oldest notification not yet shown.
pub fn take() -> Option<(String, String)> {
    without_interrupts(|| PENDING.lock().pop_front())
}

/// Up to 32 bytes of text from four words, little-endian, as far as the
/// first NUL; anything not printable ASCII becomes '?'.
pub fn unpack(words: [u64; 4]) -> String {
    words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .take_while(|byte| *byte != 0)
        .map(|byte| if (0x20..0x7f).contains(&byte) { byte as char } else { '?' })
        .collect()
}
