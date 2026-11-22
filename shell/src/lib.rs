//! A shell: command lines in, lines of output out.
//!
//! Everything a shell does that is about text and files lives here, where
//! the build host can test it: quoting and variables, pipes and `&&`,
//! file names from patterns, and the commands that read files and filter
//! text -- `ls`, `cat`, `grep`, `sort` and the rest, named and behaving as
//! on Linux wherever that makes sense on a system with no users and no
//! writable files.
//!
//! What only a running system can answer -- the clock, memory, the tasks
//! there are -- is the [`Host`]'s. The terminal is one: it gives the shell
//! its own commands, and the shell runs them in pipelines like any other.
//!
//! There are no processes under any of this yet. A pipeline's stages run
//! one after another, each handed the whole of the last one's output.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod commands;
pub mod fs;
pub mod parse;
pub mod pattern;
pub mod windows;

#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub use fs::Fs;
use parse::{Stage, When};

/// A line of what a command line produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Whether it came out as an error rather than as output.
    pub error: bool,
}

/// What a command writes: output, which a pipe hands to the next command,
/// and errors, which go straight to the screen.
#[derive(Default)]
pub struct Output {
    pub text: String,
    pub errors: Vec<String>,
}

impl Output {
    /// Write a line of output.
    pub fn line(&mut self, text: &str) {
        self.text.push_str(text);
        self.text.push('\n');
    }

    /// Write a line of error.
    pub fn error(&mut self, text: &str) {
        self.errors.push(text.to_string());
    }
}

/// A command the shell knows of: its name, how to call it, what it does.
pub type About = (&'static str, &'static str, &'static str);

/// What runs commands the shell does not have itself.
pub trait Host {
    /// The host's commands, for `help`, `which` and completion.
    fn commands(&self) -> &[About];

    /// Run `args[0]`, if it is one of the host's, and say how it went: zero
    /// for success, as for a process's exit status. `None` if it is not one.
    fn run(&mut self, args: &[String], input: &str, out: &mut Output) -> Option<i32>;
}

/// A host with no commands of its own.
pub struct NoHost;

impl Host for NoHost {
    fn commands(&self) -> &[About] {
        &[]
    }

    fn run(&mut self, _: &[String], _: &str, _: &mut Output) -> Option<i32> {
        None
    }
}

/// Where the shell is, its variables, and how the last command went.
pub struct Shell {
    cwd: String,
    vars: Vec<(String, String)>,
    status: i32,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Where `cd` with no argument goes.
pub const HOME: &str = "/home";

impl Shell {
    pub fn new() -> Self {
        Shell {
            cwd: String::from("/"),
            vars: alloc::vec![
                (String::from("HOME"), String::from(HOME)),
                (String::from("SHELL"), String::from("corrode-shell")),
            ],
            status: 0,
        }
    }

    /// Where relative paths start.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Go to `path` if it is a directory in `fs`.
    pub fn set_cwd(&mut self, fs: &Fs, path: &str) -> bool {
        let path = fs::resolve(&self.cwd, path);
        if fs.get(&path).is_some_and(|node| node.is_dir()) {
            self.cwd = path;
            true
        } else {
            false
        }
    }

    /// The last command's status, as `$?` says it.
    pub fn status(&self) -> i32 {
        self.status
    }

    pub fn var(&self, name: &str) -> Option<String> {
        match name {
            "?" => Some(self.status.to_string()),
            "PWD" => Some(self.cwd.clone()),
            _ => self
                .vars
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone()),
        }
    }

    fn set_var(&mut self, name: &str, value: &str) {
        match self.vars.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value.to_string(),
            None => self.vars.push((name.to_string(), value.to_string())),
        }
    }

    /// The names of every command there is, for completing one.
    pub fn names(&self, host: &dyn Host) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = commands::BUILTINS
            .iter()
            .chain(host.commands())
            .map(|(name, _, _)| *name)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Run a command line and say what it printed.
    pub fn run(&mut self, line: &str, fs: &Fs, host: &mut dyn Host) -> Vec<Line> {
        let mut transcript = Vec::new();
        let list = match parse::parse(line) {
            Ok(list) => list,
            Err(message) => {
                transcript.push(Line {
                    text: format!("shell: {message}"),
                    error: true,
                });
                self.status = 2;
                return transcript;
            }
        };
        for (when, pipeline) in list {
            let go = match when {
                When::Always => true,
                When::Succeeded => self.status == 0,
                When::Failed => self.status != 0,
            };
            if go {
                self.status = self.pipeline(&pipeline.stages, fs, host, &mut transcript);
            }
        }
        transcript
    }

    fn pipeline(
        &mut self,
        stages: &[Stage],
        fs: &Fs,
        host: &mut dyn Host,
        transcript: &mut Vec<Line>,
    ) -> i32 {
        let mut input = String::new();
        let mut status = 0;
        for stage in stages {
            let mut out = Output::default();
            if let Some(file) = &stage.input {
                let file = file.expand(&|name| self.var(name)).unwrap_or_default();
                let path = fs::resolve(&self.cwd, &file);
                match fs.get(&path) {
                    Some(node) if !node.is_dir() => {
                        input = String::from_utf8_lossy(node.data).into_owned();
                    }
                    Some(_) => {
                        out.error(&format!("shell: {file}: Is a directory"));
                        status = 1;
                    }
                    None => {
                        out.error(&format!("shell: {file}: No such file or directory"));
                        status = 1;
                    }
                }
            }
            if status == 0 || stage.input.is_none() {
                let args = self.expand(&stage.words, fs);
                status = self.command(&args, &input, fs, host, &mut out);
            }
            for error in out.errors {
                transcript.push(Line {
                    text: error,
                    error: true,
                });
            }
            input = out.text;
        }
        for text in input.lines() {
            transcript.push(Line {
                text: text.to_string(),
                error: false,
            });
        }
        status
    }

    /// Words to arguments: variables replaced, and a word asking for file
    /// names becomes the names it matches, in order, or stays as it is if
    /// it matches none.
    fn expand(&self, words: &[parse::Word], fs: &Fs) -> Vec<String> {
        let mut args = Vec::new();
        for word in words {
            let Some(text) = word.expand(&|name| self.var(name)) else {
                continue;
            };
            if !word.glob {
                args.push(text);
                continue;
            }
            let (dir, pattern) = match text.rfind('/') {
                Some(at) => (&text[..=at], &text[at + 1..]),
                None => ("", text.as_str()),
            };
            let base = fs::resolve(&self.cwd, if dir.is_empty() { "." } else { dir });
            let mut found: Vec<String> = fs
                .children(&base)
                .filter(|node| !node.name().starts_with('.') || pattern.starts_with('.'))
                .filter(|node| fs::glob_match(pattern, node.name()))
                .map(|node| format!("{dir}{}", node.name()))
                .collect();
            if found.is_empty() {
                args.push(text.clone());
            } else {
                found.sort();
                args.extend(found);
            }
        }
        args
    }

    fn command(
        &mut self,
        args: &[String],
        input: &str,
        fs: &Fs,
        host: &mut dyn Host,
        out: &mut Output,
    ) -> i32 {
        let Some(name) = args.first() else {
            return 0;
        };
        // `NAME=value` on its own sets a variable.
        if args.len() == 1 {
            if let Some((key, value)) = assignment(name) {
                self.set_var(key, value);
                return 0;
            }
        }
        if let Some(status) = commands::run(self, args, input, fs, host, out) {
            return status;
        }
        if let Some(status) = host.run(args, input, out) {
            return status;
        }
        out.error(&format!("{name}: command not found (try help)"));
        127
    }
}

/// `NAME=value`, if that is what `word` is.
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (key, value) = word.split_once('=')?;
    let mut chars = key.chars();
    let first = chars.next()?;
    let valid = (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    valid.then_some((key, value))
}
