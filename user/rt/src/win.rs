//! Running a Windows program from the files (`window::RUN`), as the
//! terminal's `win` and Files both do: the text sent ahead of `RUN`, then
//! the program's output read line by line until the kernel says it ended.
//!
//! The window server takes `RUN` from the terminal and Files alone, starts
//! the program seeing its own folder, and hands back the endpoint its
//! output goes to. That lands in `app::PROGRAM`, deleted again at the end.

use crate::abi::{app, log, window};
use crate::{Error, Message};

/// How a program that started came to the end of its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited, with this exit code.
    Exited(u32),
    /// It faulted, on this exception vector, and was stopped.
    Faulted(u8),
    /// It was still running when no more of it was shown.
    StillRunning,
    /// It ran 10 seconds and was stopped.
    Stopped,
    /// The output endpoint failed before the end was said.
    Lost,
}

/// Why a program did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotStarted {
    /// The text would pass `window::RUN_TEXT_MOST`.
    TooLong,
    /// The window server would not take the text.
    NotTaken,
    /// The call to the window server failed.
    NoAnswer(Error),
    /// The window server said no, with a Windows error number: 2, no such
    /// file; 5, the caller may not run one; 193, not a program.
    Refused(u64),
}

impl NotStarted {
    /// Why, as `win` says it.
    pub fn why(&self) -> &'static str {
        match self {
            NotStarted::TooLong => "the command line is too long",
            NotStarted::NotTaken => "the desktop would not take the command line",
            NotStarted::NoAnswer(_) => "the desktop did not answer",
            NotStarted::Refused(2) => "no such file",
            NotStarted::Refused(5) => "only the terminal and Files may run one",
            NotStarted::Refused(193) => "not a program Verdigris runs",
            NotStarted::Refused(_) => "not started",
        }
    }
}

/// Run what `text` says -- the program's absolute path, a line feed, and
/// its arguments quoted as on Windows -- and give `each` its output: each
/// piece of text, and whether it ends a line.
pub fn run(text: &str, mut each: impl FnMut(&str, bool)) -> Result<Ended, NotStarted> {
    if text.len() > window::RUN_TEXT_MOST {
        return Err(NotStarted::TooLong);
    }
    for chunk in text.as_bytes().chunks(32) {
        let mut bytes = [0u8; 32];
        bytes[..chunk.len()].copy_from_slice(chunk);
        let taken = crate::call(app::WINDOWS, Message::new(window::RUN_TEXT, words_of(&bytes)), None);
        if !taken.is_ok_and(|answer| answer.message.words[0] == 1) {
            return Err(NotStarted::NotTaken);
        }
    }
    let _ = crate::delete(app::PROGRAM);
    let answer = crate::call(app::WINDOWS, Message::new(window::RUN, [0; 4]), Some(app::PROGRAM))
        .map_err(NotStarted::NoAnswer)?
        .message
        .words;
    if answer[0] != 1 {
        return Err(NotStarted::Refused(answer[1]));
    }
    let ended = loop {
        let Ok(received) = crate::recv(app::PROGRAM, None, None) else {
            break Ended::Lost;
        };
        let words = received.message.words;
        let mut bytes = [0u8; 32];
        for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(words) {
            *chunk = word.to_le_bytes();
        }
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        let piece = core::str::from_utf8(&bytes[..end]).unwrap_or("?");
        match received.message.label {
            log::TEXT => each(piece, false),
            log::LINE => each(piece, true),
            log::END => {
                break match words[0] {
                    log::END_RUNNING => Ended::StillRunning,
                    log::END_STOPPED => Ended::Stopped,
                    log::END_FAULTED => Ended::Faulted(words[1] as u8),
                    _ => Ended::Exited(words[1] as u32),
                };
            }
            _ => {}
        }
    };
    let _ = crate::delete(app::PROGRAM);
    Ok(ended)
}

/// A fault as Windows names the exception, and the exception code, which
/// is also the exit code Windows gives a program it ended for one.
pub fn exception(vector: u8) -> (&'static str, u32) {
    match vector {
        0 => ("integer divide by zero", 0xc000_0094),
        3 => ("breakpoint", 0x8000_0003),
        6 => ("illegal instruction", 0xc000_001d),
        13 | 14 => ("access violation", 0xc000_0005),
        17 => ("misaligned data", 0x8000_0002),
        _ => ("unknown exception", 0xc000_0001),
    }
}

/// 32 bytes as four little-endian words.
fn words_of(bytes: &[u8; 32]) -> [u64; 4] {
    let mut words = [0u64; 4];
    for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*chunk);
    }
    words
}
