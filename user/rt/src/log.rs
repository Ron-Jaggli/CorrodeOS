//! Saying things.
//!
//! Printing is not something a program can do. It is something a program
//! can ask for, if it was handed a send-only endpoint to the log -- and the
//! request is a message like any other, thirty-two bytes at a time, since
//! nothing longer fits in registers and nothing else crosses the boundary.
//!
//! [`set`] names the slot once; `println!` then works. Before that, and in
//! a program that was given no log at all, it does nothing: there is nowhere
//! for the text to go, which is the correct amount of output for a task
//! with no capability to produce any.

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::abi::log::{CAPACITY, LINE, TEXT};
use crate::{send, Message};

const UNSET: usize = usize::MAX;

/// The slot holding this program's log endpoint, or [`UNSET`].
static SLOT: AtomicUsize = AtomicUsize::new(UNSET);

/// Say which slot holds the log endpoint.
pub fn set(slot: usize) {
    SLOT.store(slot, Ordering::Relaxed);
}

/// One line on its way to the log.
///
/// Bytes collect in a message's worth of buffer and go out when it fills.
/// The line itself goes out when the writer is dropped.
pub struct Line {
    slot: usize,
    buffer: [u8; CAPACITY],
    len: usize,
}

impl Line {
    /// Start a line, to wherever [`set`] said.
    pub fn new() -> Self {
        Line {
            slot: SLOT.load(Ordering::Relaxed),
            buffer: [0; CAPACITY],
            len: 0,
        }
    }

    fn flush(&mut self, label: u64) {
        if self.slot == UNSET {
            return;
        }
        let mut words = [0u64; 4];
        for (i, byte) in self.buffer[..self.len].iter().enumerate() {
            words[i / 8] |= (*byte as u64) << ((i % 8) * 8);
        }
        // A log that refuses us is not worth stopping for.
        let _ = send(self.slot, Message::new(label, words));
        self.len = 0;
    }
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            // Zero ends a message's text, and a line is one line.
            let byte = match byte {
                0 | b'\n' | b'\r' => b' ',
                other => other,
            };
            if self.len == CAPACITY {
                self.flush(TEXT);
            }
            self.buffer[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        self.flush(LINE);
    }
}

/// Print a line to the log, if this program has one.
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = $crate::log::Line::new();
        let _ = write!(line, $($arg)*);
    }};
}
