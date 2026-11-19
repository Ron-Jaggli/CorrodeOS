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

/// The last few notifications, newest first, with when they came, for
/// Control Center's list. Kept whether or not they were ever shown.
static HISTORY: SpinLock<VecDeque<(String, String, u64)>> = SpinLock::new(VecDeque::new());
const KEPT: usize = 5;

/// File a notification from `from` saying `text`.
pub fn post(from: String, text: String) {
    crate::println!("notify     : {from}: {text}");
    without_interrupts(|| {
        let mut pending = PENDING.lock();
        if pending.len() == MOST {
            pending.pop_front();
        }
        pending.push_back((from.clone(), text.clone()));
        let mut history = HISTORY.lock();
        if history.len() == KEPT {
            history.pop_back();
        }
        history.push_front((from, text, crate::time::now_ns()));
    });
}

/// The oldest notification not yet shown.
pub fn take() -> Option<(String, String)> {
    without_interrupts(|| PENDING.lock().pop_front())
}

/// How many recent notifications there are, and the `which`th newest:
/// from whom, what, and how many seconds ago.
pub fn recent(which: usize) -> (usize, Option<(String, String, u64)>) {
    let now = crate::time::now_ns();
    without_interrupts(|| {
        let history = HISTORY.lock();
        let entry = history
            .get(which)
            .map(|(from, text, at)| (from.clone(), text.clone(), now.saturating_sub(*at) / 1_000_000_000));
        (history.len(), entry)
    })
}

/// Text into four words, little-endian, up to 32 bytes: [`unpack`]'s
/// other half.
pub fn pack(text: &str) -> [u64; 4] {
    let mut words = [0u64; 4];
    for (index, byte) in text.bytes().take(32).enumerate() {
        words[index / 8] |= (byte as u64) << (index % 8 * 8);
    }
    words
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
