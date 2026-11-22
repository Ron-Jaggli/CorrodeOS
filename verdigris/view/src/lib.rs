//! A Windows program's files: the real tree, pruned (`verdigris/DESIGN.md`,
//! section 4).
//!
//! A program is allowed its own folder and a list of other paths, each a
//! directory or a file. It sees the real files through a view of them:
//!
//! - an allowed directory shows everything in it, an allowed file itself;
//! - a directory on the way to something allowed shows only what leads to
//!   something allowed;
//! - anything else is not there. Opening it fails exactly as for a name
//!   that does not exist (`ERROR_FILE_NOT_FOUND`, `ERROR_PATH_NOT_FOUND`),
//!   never with "access denied", so a program cannot tell a hidden file
//!   from a missing one.
//!
//! `C:\` is the view's root, the deepest directory holding everything
//! allowed, so a program learns no more of the tree above it than it must.
//! Paths are Windows paths: `\` or `/`, an optional `C:` and `\\?\`,
//! `.` and `..` (`..` at the root stays there), names matched without
//! regard to case. Other drives, UNC and device paths are not files.
//!
//! The tree is read-only today (the archive packed from `root/`), so
//! everything in a view is read-only media: asking to write or to create
//! where the view shows a directory fails with `ERROR_WRITE_PROTECT`.
//!
//! This is the whole of the rule. The file service that holds the real
//! tree answers every call through it, and the host tests check it against
//! DESIGN.md's examples.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use corrode_shell::fs::{Fs, Node};

/// Why a path opens nothing, as the Windows error a program is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The last name is not there (or not in the view).
    FileNotFound,
    /// A directory on the way is not there, or the path is not a file
    /// path at all.
    PathNotFound,
    /// A directory, opened as a file: what Windows says to that.
    AccessDenied,
    /// Writing or creating, where the view is read-only media.
    WriteProtect,
    /// A creation disposition Windows does not have.
    InvalidParameter,
    /// A file, where a directory was asked for (`ERROR_DIRECTORY`).
    NotADirectory,
}

impl Error {
    /// `GetLastError`'s number for it.
    pub fn code(self) -> u32 {
        match self {
            Error::FileNotFound => 2,
            Error::PathNotFound => 3,
            Error::AccessDenied => 5,
            Error::WriteProtect => 19,
            Error::InvalidParameter => 87,
            Error::NotADirectory => 267,
        }
    }
}

/// `CreateFileW`'s creation dispositions.
pub const CREATE_NEW: u32 = 1;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const OPEN_ALWAYS: u32 = 4;
pub const TRUNCATE_EXISTING: u32 = 5;

/// One entry a search finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

/// Whether `name` matches a Windows wildcard `pattern`: `*` any run of
/// characters, `?` one, the rest without regard to ASCII case. `*.*`
/// matches every name, with a dot or without, as on Windows. Greedy with
/// one point to go back to, so a pattern of many stars costs no more than
/// its length times the name's.
pub fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern = if pattern == "*.*" { "*" } else { pattern };
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    let (mut p, mut n) = (0, 0);
    let mut back: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                back = Some((p, n));
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                n += 1;
            }
            Some(c) if c.eq_ignore_ascii_case(&name[n]) => {
                p += 1;
                n += 1;
            }
            _ => match back {
                // The last star takes one more character.
                Some((star, from)) => {
                    p = star + 1;
                    n = from + 1;
                    back = Some((star, from + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == b'*')
}

/// What one program may see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// Absolute real paths, its own folder first.
    allowed: Vec<String>,
    /// The real directory that is `C:\`.
    root: String,
}

/// How a real path shows in a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Allowed, or inside something allowed: as it is.
    Whole,
    /// A directory leading to something allowed: only those entries.
    OnTheWay,
    Hidden,
}

/// Whether `path` is `dir` or inside it.
fn within(path: &str, dir: &str) -> bool {
    path == dir || dir == "/" || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// The names in an absolute real path.
fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|part| !part.is_empty())
}

fn join(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// A path as given, made absolute and clean: `/`, no trailing or doubled
/// slashes, no `.` or `..`.
fn clean(path: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for part in parts(path) {
        match part {
            "." => {}
            ".." => {
                kept.pop();
            }
            part => kept.push(part),
        }
    }
    join(&kept)
}

impl View {
    /// The view of a program in `folder`, allowed `allowed` as well.
    pub fn new(folder: &str, allowed: &[&str]) -> View {
        let allowed: Vec<String> = core::iter::once(folder).chain(allowed.iter().copied()).map(clean).collect();
        // The deepest directory every allowed path is in, or is.
        let mut common: Vec<&str> = parts(&allowed[0]).collect();
        for path in &allowed[1..] {
            let same = common.iter().zip(parts(path)).take_while(|(a, b)| **a == *b).count();
            common.truncate(same);
        }
        let root = join(&common);
        View { allowed, root }
    }

    /// The real directory that is `C:\`.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The program's own folder, where relative paths start.
    pub fn home(&self) -> &str {
        &self.allowed[0]
    }

    fn seen(&self, path: &str) -> Seen {
        if !within(path, &self.root) {
            return Seen::Hidden;
        }
        if self.allowed.iter().any(|allowed| within(path, allowed)) {
            Seen::Whole
        } else if self.allowed.iter().any(|allowed| within(allowed, path)) {
            Seen::OnTheWay
        } else {
            Seen::Hidden
        }
    }

    /// The real tree's root, made the view's: if the deepest common
    /// directory is in fact a file (one allowed file and no folder), the
    /// directory it is in.
    fn root_in<'f, 'a>(&self, fs: &'f Fs<'a>) -> Option<&'f Node<'a>> {
        let node = fs.get(&self.root)?;
        if node.is_dir() {
            Some(node)
        } else {
            fs.get(corrode_shell::fs::parent_of(&self.root)?)
        }
    }

    /// The entries of the real directory `dir` that the view shows, in
    /// the tree's (sorted) order.
    fn shown<'f, 'a>(&self, fs: &'f Fs<'a>, dir: &str) -> Vec<&'f Node<'a>> {
        let paths: Vec<String> = fs
            .children(dir)
            .filter(|node| self.seen(&node.path) != Seen::Hidden)
            .map(|node| node.path.clone())
            .collect();
        paths.iter().filter_map(|path| fs.get(path)).collect()
    }

    /// Where a Windows path leads in the real tree: the node it names, or
    /// why it names nothing. `cwd` is the current directory, a real path in
    /// the view (the program's folder unless it changed it).
    pub fn find<'f, 'a>(&self, fs: &'f Fs<'a>, cwd: &str, path: &str) -> Result<&'f Node<'a>, Error> {
        let names = self.names(cwd, path)?;
        let mut at = self.root_in(fs).ok_or(Error::PathNotFound)?;
        for (index, name) in names.iter().enumerate() {
            let last = index + 1 == names.len();
            if !at.is_dir() {
                return Err(Error::PathNotFound);
            }
            let mut first_folded = None;
            let mut exact = None;
            for child in self.shown(fs, &at.path.clone()) {
                if child.name() == name {
                    exact = Some(child);
                    break;
                }
                if first_folded.is_none() && child.name().eq_ignore_ascii_case(name) {
                    first_folded = Some(child);
                }
            }
            match exact.or(first_folded) {
                Some(child) => at = child,
                None if last => return Err(Error::FileNotFound),
                None => return Err(Error::PathNotFound),
            }
        }
        Ok(at)
    }

    /// `CreateFileW`, as far as the view decides it: the file to open, or
    /// the error. `write` is whether any right to change it was asked for.
    pub fn open<'f, 'a>(
        &self,
        fs: &'f Fs<'a>,
        cwd: &str,
        path: &str,
        write: bool,
        disposition: u32,
    ) -> Result<&'f Node<'a>, Error> {
        if !(CREATE_NEW..=TRUNCATE_EXISTING).contains(&disposition) {
            return Err(Error::InvalidParameter);
        }
        let found = self.find(fs, cwd, path);
        let creates = matches!(disposition, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING)
            || (disposition == OPEN_ALWAYS && found == Err(Error::FileNotFound));
        if write || creates {
            // A name not there, in a directory the view shows: that
            // directory is read-only media. Anything further out is not
            // there at all.
            return match found {
                Ok(_) | Err(Error::FileNotFound) => Err(Error::WriteProtect),
                Err(error) => Err(error),
            };
        }
        let node = found?;
        if node.is_dir() {
            return Err(Error::AccessDenied);
        }
        Ok(node)
    }

    /// `FindFirstFileW`: the entries a pattern finds, in order. The last
    /// name of `path` is the pattern (`C:\etc\*`, `*.txt`); the rest names
    /// the directory, which must be in the view (or it is
    /// `ERROR_PATH_NOT_FOUND`, hidden or missing alike). A directory other
    /// than the root shows `.` and `..` first, as on Windows. Nothing
    /// found is `ERROR_FILE_NOT_FOUND`.
    pub fn search(&self, fs: &Fs<'_>, cwd: &str, path: &str) -> Result<Vec<Found>, Error> {
        let path: String = path.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
        let (dir, pattern) = match path.rfind('\\') {
            Some(at) => (&path[..=at], &path[at + 1..]),
            None if path.as_bytes().get(1) == Some(&b':') => (&path[..2], &path[2..]),
            None => ("", path.as_str()),
        };
        if pattern.is_empty() {
            return Err(Error::FileNotFound);
        }
        let dir = match self.find(fs, cwd, if dir.is_empty() { "." } else { dir }) {
            Ok(dir) if dir.is_dir() => dir,
            Ok(_) | Err(_) => return Err(Error::PathNotFound),
        };
        let mut found = Vec::new();
        if self.root_in(fs).is_some_and(|root| root.path != dir.path) {
            for dots in [".", ".."] {
                found.push(Found { name: String::from(dots), dir: true, size: 0 });
            }
        }
        for node in self.shown(fs, &dir.path) {
            found.push(Found {
                name: String::from(node.name()),
                dir: node.is_dir(),
                size: if node.is_dir() { 0 } else { node.data.len() as u64 },
            });
        }
        found.retain(|entry| wildcard(pattern, &entry.name));
        if found.is_empty() {
            return Err(Error::FileNotFound);
        }
        Ok(found)
    }

    /// A real path in the view as the program is shown it: `C:\` and the
    /// names below the view's root. Where the view sits in the real tree
    /// is never in it.
    pub fn windows_path(&self, real: &str) -> String {
        let below = real.strip_prefix(self.root.as_str()).unwrap_or("");
        let mut out = String::from("C:");
        for name in parts(below) {
            out.push('\\');
            out.push_str(name);
        }
        if out.len() == 2 {
            out.push('\\');
        }
        out
    }

    /// `GetFullPathNameW`: `path` made absolute against `cwd` and cleaned,
    /// as a `C:\` path. Like Windows, it asks nothing of the tree: a name
    /// that is not there gets a full path too, so it says nothing about
    /// what exists. A trailing `\` is kept.
    pub fn full_path(&self, cwd: &str, path: &str) -> Result<String, Error> {
        let names = self.names(cwd, path)?;
        let mut out = String::from("C:");
        for name in &names {
            out.push('\\');
            out.push_str(name);
        }
        if names.is_empty() || (path.ends_with(['\\', '/']) && path.len() > 1) {
            out.push('\\');
        }
        Ok(out)
    }

    /// `SetCurrentDirectoryW`: the real directory `path` names, to be the
    /// new `cwd`. Hidden and missing alike fail as for a missing name; a
    /// file is `ERROR_DIRECTORY`.
    pub fn change_dir(&self, fs: &Fs<'_>, cwd: &str, path: &str) -> Result<String, Error> {
        let node = self.find(fs, cwd, path)?;
        if !node.is_dir() {
            return Err(Error::NotADirectory);
        }
        Ok(node.path.clone())
    }

    /// The names a directory shows, for `path` naming one.
    pub fn list<'f>(&self, fs: &'f Fs<'_>, cwd: &str, path: &str) -> Result<Vec<&'f str>, Error> {
        let dir = self.find(fs, cwd, path)?;
        if !dir.is_dir() {
            return Err(Error::PathNotFound);
        }
        Ok(self.shown(fs, &dir.path).into_iter().map(|node| node.name()).collect())
    }

    /// A Windows path as names from the view's root, `.` and `..` gone.
    fn names(&self, cwd: &str, path: &str) -> Result<Vec<String>, Error> {
        let path: String = path.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
        let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
        if path.starts_with(r"\\") {
            // UNC (`\\server\share`) and devices (`\\.\`): not files.
            return Err(Error::PathNotFound);
        }
        let bytes = path.as_bytes();
        let (absolute, rest) = if bytes.len() >= 2 && bytes[1] == b':' {
            if !bytes[0].eq_ignore_ascii_case(&b'c') {
                return Err(Error::PathNotFound);
            }
            let rest = &path[2..];
            (rest.starts_with('\\'), rest)
        } else {
            (path.starts_with('\\'), path)
        };
        let mut names: Vec<String> = Vec::new();
        if !absolute {
            let start = cwd.strip_prefix(&self.root).unwrap_or("");
            names.extend(parts(start).map(String::from));
        }
        for name in rest.split('\\') {
            match name {
                "" | "." => {}
                ".." => {
                    names.pop();
                }
                name => names.push(String::from(name)),
            }
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests;
