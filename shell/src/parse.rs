//! A command line, as words in pipelines joined by `;`, `&&` and `||`.
//!
//! The rules are a small, honest subset of a POSIX shell's: single quotes
//! keep everything, double quotes keep everything but `$` and `\`, a
//! backslash outside quotes keeps the next character, `$NAME`, `${NAME}`
//! and `$?` name variables, `#` at the start of a word begins a comment, and
//! `< file` feeds a file to a command. What a shell has that this one does
//! not -- writing files, running things in the background -- is a syntax
//! error saying so, not a silent misreading.
//!
//! Variables are kept as names here and looked up by [`Word::expand`] when
//! the command they are in runs, so `X=1; echo $X` sees the 1.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One word: text and the variables in it, whether any of it was quoted,
/// and whether an unquoted `*` or `?` in it asks for file names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Word {
    pub pieces: Vec<Piece>,
    pub quoted: bool,
    pub glob: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Var(String),
}

impl Word {
    /// The word, its variables replaced by `lookup`. `None` if it was
    /// nothing but unquoted variables with nothing in them: such a word is
    /// not there at all, as in any shell.
    pub fn expand(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        let mut text = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(part) => text.push_str(part),
                Piece::Var(name) => text.extend(lookup(name)),
            }
        }
        let vanished = text.is_empty()
            && !self.quoted
            && self
                .pieces
                .iter()
                .all(|piece| matches!(piece, Piece::Var(_)));
        (!vanished).then_some(text)
    }

    fn extend(&mut self, c: Option<char>) {
        if let Some(c) = c {
            self.push(c);
        }
    }

    fn push(&mut self, c: char) {
        match self.pieces.last_mut() {
            Some(Piece::Text(text)) => text.push(c),
            _ => self.pieces.push(Piece::Text(String::from(c))),
        }
    }
}

/// One command: its words, and a file for its input if it was given one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stage {
    pub words: Vec<Word>,
    pub input: Option<Word>,
}

/// Commands joined by `|`, each one's output the next one's input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pipeline {
    pub stages: Vec<Stage>,
}

/// When a pipeline runs, given how the one before it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// First, or after `;`.
    Always,
    /// After `&&`: if the last one succeeded.
    Succeeded,
    /// After `||`: if the last one failed.
    Failed,
}

/// Split `line` into pipelines.
pub fn parse(line: &str) -> Result<Vec<(When, Pipeline)>, String> {
    let mut parser = Parser {
        chars: line.chars().collect(),
        at: 0,
    };
    parser.list()
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

/// What ends a word, unquoted.
fn special(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>')
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.at += 1;
        }
    }

    fn list(&mut self) -> Result<Vec<(When, Pipeline)>, String> {
        let mut list = Vec::new();
        let mut when = When::Always;
        loop {
            let pipeline = self.pipeline()?;
            self.skip_spaces();
            let next = match (self.peek(), self.peek_at(1)) {
                (None, _) => None,
                (Some(';'), _) => Some((When::Always, 1)),
                (Some('&'), Some('&')) => Some((When::Succeeded, 2)),
                (Some('|'), Some('|')) => Some((When::Failed, 2)),
                (Some('&'), _) => {
                    return Err("there are no background jobs: & is not supported".into())
                }
                (Some(c), _) => return Err(format!("syntax error near {c}")),
            };
            let empty = pipeline.stages.is_empty();
            match next {
                None if empty && list.is_empty() => return Ok(list),
                None if empty && when == When::Always => return Ok(list),
                None if empty => return Err("syntax error: nothing after && or ||".into()),
                None => {
                    list.push((when, pipeline));
                    return Ok(list);
                }
                Some((_, _)) if empty => {
                    return Err("syntax error: a command is missing".into());
                }
                Some((then, length)) => {
                    list.push((when, pipeline));
                    when = then;
                    self.at += length;
                }
            }
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline, String> {
        let mut pipeline = Pipeline::default();
        loop {
            let stage = self.stage()?;
            self.skip_spaces();
            let piped = self.peek() == Some('|') && self.peek_at(1) != Some('|');
            if stage.words.is_empty() {
                if piped || !pipeline.stages.is_empty() {
                    return Err("syntax error: a command is missing around |".into());
                }
                if stage.input.is_some() {
                    return Err("syntax error: < with no command".into());
                }
                return Ok(pipeline);
            }
            pipeline.stages.push(stage);
            if !piped {
                return Ok(pipeline);
            }
            self.at += 1;
        }
    }

    fn stage(&mut self) -> Result<Stage, String> {
        let mut stage = Stage::default();
        loop {
            self.skip_spaces();
            match self.peek() {
                None => return Ok(stage),
                Some('#') => {
                    self.at = self.chars.len();
                    return Ok(stage);
                }
                Some('>') => {
                    return Err("cannot write files: the file system is read-only".into());
                }
                Some('<') => {
                    self.at += 1;
                    self.skip_spaces();
                    match self.word()? {
                        Some(word) => stage.input = Some(word),
                        None => return Err("syntax error: < needs a file".into()),
                    }
                }
                Some(c) if special(c) => return Ok(stage),
                Some(_) => {
                    if let Some(word) = self.word()? {
                        stage.words.push(word);
                    }
                }
            }
        }
    }

    /// Read one word, or `None` if there is none here.
    fn word(&mut self) -> Result<Option<Word>, String> {
        let mut word = Word::default();
        let mut started = false;
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' => break,
                c if special(c) => break,
                '\'' => {
                    self.at += 1;
                    started = true;
                    word.quoted = true;
                    loop {
                        match self.peek() {
                            None => return Err("syntax error: a ' is not closed".into()),
                            Some('\'') => break,
                            Some(c) => word.push(c),
                        }
                        self.at += 1;
                    }
                    self.at += 1;
                }
                '"' => {
                    self.at += 1;
                    started = true;
                    word.quoted = true;
                    loop {
                        match self.peek() {
                            None => return Err("syntax error: a \" is not closed".into()),
                            Some('"') => break,
                            Some('\\') if matches!(self.peek_at(1), Some('"' | '\\' | '$')) => {
                                self.at += 1;
                                word.extend(self.peek());
                            }
                            Some('$') => {
                                self.variable(&mut word);
                                continue;
                            }
                            Some(c) => word.push(c),
                        }
                        self.at += 1;
                    }
                    self.at += 1;
                }
                '\\' => {
                    self.at += 1;
                    started = true;
                    word.quoted = true;
                    if let Some(c) = self.peek() {
                        word.push(c);
                        self.at += 1;
                    }
                }
                '$' => {
                    started = true;
                    self.variable(&mut word);
                }
                _ => {
                    started = true;
                    word.glob |= c == '*' || c == '?';
                    word.push(c);
                    self.at += 1;
                }
            }
        }
        Ok(started.then_some(word))
    }

    /// At a `$`: add the variable it names to `word`, and move past it. A
    /// `$` that names nothing is itself.
    fn variable(&mut self, word: &mut Word) {
        self.at += 1;
        let name = match self.peek() {
            Some('?') => {
                self.at += 1;
                String::from("?")
            }
            Some('{') => {
                let close = self.chars[self.at..].iter().position(|c| *c == '}');
                let Some(close) = close else {
                    word.push('$');
                    return;
                };
                let name: String = self.chars[self.at + 1..self.at + close].iter().collect();
                self.at += close + 1;
                name
            }
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(c) = self
                    .peek()
                    .filter(|c| *c == '_' || c.is_ascii_alphanumeric())
                {
                    name.push(c);
                    self.at += 1;
                }
                name
            }
            _ => {
                word.push('$');
                return;
            }
        };
        word.pieces.push(Piece::Var(name));
    }
}
