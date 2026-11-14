//! A line being typed: where the cursor is, and the lines typed before it.
//!
//! What a shell's prompt does with a key -- move along the line, step back
//! through history, cut a word -- is arithmetic on a string and needs no
//! window to check. So it lives here, fed one key at a time, and the tests
//! run on the build host. The terminal draws what it is told and runs what
//! it is handed.
//!
//! The keys are the ones a Linux shell's fingers expect: arrows, Home, End,
//! Delete, and Emacs-style Ctrl chords for the rest.
//!
//! The line holds printable ASCII and nothing else, so a byte index is a
//! character index and the cursor can never land inside a character.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::vec::Vec;

use corrode_input::{modifier, Key};

/// Characters a line may hold.
pub const MAX: usize = 120;
/// Lines of history kept. Older ones are dropped.
pub const HISTORY: usize = 100;

/// What a key asks of whoever owns the prompt, beyond redrawing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing more: the line or the cursor may have changed.
    Edited,
    /// Enter: run this line. The prompt is empty again.
    Submit(String),
    /// Ctrl+C: abandon this line, which was this.
    Cancel(String),
    /// Ctrl+L: clear the screen, keeping the line.
    ClearScreen,
    /// Tab: complete the word under the cursor.
    Complete,
    /// The key does nothing here.
    Ignored,
}

/// A line being edited, and the history behind it.
#[derive(Default)]
pub struct Editor {
    line: String,
    /// Byte offset into `line`; at most its length.
    cursor: usize,
    history: Vec<String>,
    /// Which history entry is shown, while stepping through it.
    recalled: Option<usize>,
    /// What was being typed before stepping into history.
    draft: String,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn line(&self) -> &str {
        &self.line
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Lines run so far, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Feed a key press. Releases should not be fed.
    pub fn key(&mut self, key: Key, text: Option<char>, modifiers: u8) -> Outcome {
        if modifiers & modifier::CTRL != 0 {
            return match text.map(|c| c.to_ascii_lowercase()) {
                Some('a') => self.go(0),
                Some('e') => self.go(self.line.len()),
                Some('b') => self.go(self.cursor.saturating_sub(1)),
                Some('f') => self.go(self.cursor + 1),
                Some('d') => self.delete(),
                Some('u') => self.cut(0, self.cursor),
                Some('k') => self.cut(self.cursor, self.line.len()),
                Some('w') => self.cut(self.word_start(), self.cursor),
                Some('p') => self.older(),
                Some('n') => self.newer(),
                Some('l') => Outcome::ClearScreen,
                Some('c') => {
                    let line = self.take();
                    Outcome::Cancel(line)
                }
                _ => Outcome::Ignored,
            };
        }
        match key {
            Key::LEFT => self.go(self.cursor.saturating_sub(1)),
            Key::RIGHT => self.go(self.cursor + 1),
            Key::HOME => self.go(0),
            Key::END => self.go(self.line.len()),
            Key::UP => self.older(),
            Key::DOWN => self.newer(),
            Key::DELETE => self.delete(),
            Key::BACKSPACE => {
                if self.cursor == 0 {
                    return Outcome::Ignored;
                }
                self.cursor -= 1;
                self.line.remove(self.cursor);
                Outcome::Edited
            }
            Key::ENTER => {
                let line = self.take();
                let trimmed = line.trim();
                let repeat = self.history.last().is_some_and(|last| last == trimmed);
                if !trimmed.is_empty() && !repeat {
                    if self.history.len() == HISTORY {
                        self.history.remove(0);
                    }
                    self.history.push(String::from(trimmed));
                }
                Outcome::Submit(line)
            }
            Key::TAB => Outcome::Complete,
            _ => match text {
                Some(c @ ' '..='~') => self.insert(c),
                _ => Outcome::Ignored,
            },
        }
    }

    /// Complete the first word from `names`, if the cursor is at its end.
    ///
    /// One match fills it in, with a space after. Several fill in what they
    /// share and come back, so they can be shown. None changes nothing.
    pub fn complete<'a>(&mut self, names: &[&'a str]) -> Vec<&'a str> {
        let word = &self.line[..self.cursor];
        if word.contains(' ') {
            return Vec::new();
        }
        let matches: Vec<&'a str> = names
            .iter()
            .copied()
            .filter(|name| name.starts_with(word))
            .collect();
        let Some(first) = matches.first() else {
            return matches;
        };
        let shared = matches.iter().fold(first.len(), |shared, name| {
            first
                .bytes()
                .zip(name.bytes())
                .take(shared)
                .take_while(|(a, b)| a == b)
                .count()
        });
        let mut fill = String::from(&first[word.len()..shared]);
        if matches.len() == 1 && !self.line[self.cursor..].starts_with(' ') {
            fill.push(' ');
        }
        for c in fill.chars() {
            self.insert(c);
        }
        if matches.len() == 1 {
            Vec::new()
        } else {
            matches
        }
    }

    fn go(&mut self, to: usize) -> Outcome {
        self.cursor = to.min(self.line.len());
        Outcome::Edited
    }

    fn insert(&mut self, c: char) -> Outcome {
        if self.line.len() >= MAX {
            return Outcome::Ignored;
        }
        self.line.insert(self.cursor, c);
        self.cursor += 1;
        Outcome::Edited
    }

    fn delete(&mut self) -> Outcome {
        if self.cursor == self.line.len() {
            return Outcome::Ignored;
        }
        self.line.remove(self.cursor);
        Outcome::Edited
    }

    /// Remove `from..to` and leave the cursor where it was cut.
    fn cut(&mut self, from: usize, to: usize) -> Outcome {
        self.line.replace_range(from..to, "");
        self.cursor = from;
        Outcome::Edited
    }

    /// Where the word before the cursor starts, skipping spaces before it.
    fn word_start(&self) -> usize {
        let before = self.line[..self.cursor].trim_end_matches(' ');
        before.rfind(' ').map_or(0, |space| space + 1)
    }

    /// Empty the line, leave history, and hand back what was on it.
    fn take(&mut self) -> String {
        self.recalled = None;
        self.draft.clear();
        self.cursor = 0;
        core::mem::take(&mut self.line)
    }

    fn older(&mut self) -> Outcome {
        let index = match self.recalled {
            Some(0) => return Outcome::Ignored,
            Some(index) => index - 1,
            None if self.history.is_empty() => return Outcome::Ignored,
            None => {
                self.draft = core::mem::take(&mut self.line);
                self.history.len() - 1
            }
        };
        self.show(Some(index))
    }

    fn newer(&mut self) -> Outcome {
        match self.recalled {
            None => Outcome::Ignored,
            Some(index) if index + 1 < self.history.len() => self.show(Some(index + 1)),
            Some(_) => self.show(None),
        }
    }

    /// Put a history entry on the line, or the draft back for `None`.
    fn show(&mut self, entry: Option<usize>) -> Outcome {
        self.recalled = entry;
        self.line = match entry {
            Some(index) => self.history[index].clone(),
            None => core::mem::take(&mut self.draft),
        };
        self.cursor = self.line.len();
        Outcome::Edited
    }
}
