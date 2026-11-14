//! A read-only tree of files, out of a tar archive.
//!
//! The files a terminal can read come from the boot medium, like everything
//! else: the build packs `root/` into a ustar archive and the bootloader
//! loads it. Nothing is unpacked: a file's contents are a slice of the
//! archive where it lies.
//!
//! The archive is read as numbers that might lie. Every header's checksum
//! is checked and every length is checked against what is left, so a bad
//! archive is an error, never a read past its end.

use alloc::string::String;
use alloc::vec::Vec;

const BLOCK: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// A file or a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    /// Absolute, with no trailing slash: `/`, `/etc`, `/etc/motd`.
    pub path: String,
    pub kind: Kind,
    /// The permission bits the archive gave it.
    pub mode: u32,
    pub data: &'a [u8],
}

impl Node<'_> {
    pub fn name(&self) -> &str {
        name_of(&self.path)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

/// Why an archive could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A header's checksum is wrong: not a tar archive, or a damaged one.
    Checksum,
    /// A header or a file runs past the end.
    Truncated,
    /// A number in a header is not a number.
    Number,
}

/// Every file and directory, sorted by path.
#[derive(Clone, Debug, Default)]
pub struct Fs<'a> {
    nodes: Vec<Node<'a>>,
}

impl<'a> Fs<'a> {
    /// A tree with nothing in it but `/`.
    pub fn empty() -> Self {
        let mut fs = Fs { nodes: Vec::new() };
        fs.add_dir("/");
        fs
    }

    /// Read a ustar archive.
    ///
    /// Regular files and directories are kept; links and anything else are
    /// left out. Directories an entry is inside are made if the archive
    /// does not have them.
    pub fn from_tar(bytes: &'a [u8]) -> Result<Self, Error> {
        let mut fs = Fs::empty();
        let mut at = 0;
        while at + BLOCK <= bytes.len() {
            let header = &bytes[at..at + BLOCK];
            if header.iter().all(|b| *b == 0) {
                break;
            }
            let stored = octal(&header[148..156])?;
            let sum: u64 = header
                .iter()
                .enumerate()
                .map(|(i, b)| if (148..156).contains(&i) { b' ' } else { *b } as u64)
                .sum();
            if stored != sum {
                return Err(Error::Checksum);
            }
            let size = octal(&header[124..136])? as usize;
            let mode = octal(&header[100..108])? as u32;
            let start = at + BLOCK;
            let end = start.checked_add(size).ok_or(Error::Truncated)?;
            if end > bytes.len() {
                return Err(Error::Truncated);
            }
            let mut name = String::new();
            if &header[257..262] == b"ustar" {
                let prefix = text(&header[345..500]);
                if !prefix.is_empty() {
                    name.push_str(&prefix);
                    name.push('/');
                }
            }
            name.push_str(&text(&header[0..100]));
            let path = resolve("/", &name);
            match header[156] {
                b'0' | 0 if path != "/" => fs.add(Node {
                    path,
                    kind: Kind::File,
                    mode,
                    data: &bytes[start..end],
                }),
                b'5' => {
                    fs.add_dir(&path);
                    if let Some(node) = fs.nodes.iter_mut().find(|node| node.path == path) {
                        node.mode = mode;
                    }
                }
                _ => {}
            }
            at = start + size.div_ceil(BLOCK) * BLOCK;
        }
        fs.nodes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(fs)
    }

    fn add(&mut self, node: Node<'a>) {
        if let Some(parent) = parent_of(&node.path) {
            self.add_dir(parent);
        }
        self.nodes.retain(|old| old.path != node.path);
        self.nodes.push(node);
    }

    fn add_dir(&mut self, path: &str) {
        if self.nodes.iter().any(|node| node.path == path) {
            return;
        }
        if let Some(parent) = parent_of(path) {
            self.add_dir(parent);
        }
        self.nodes.push(Node {
            path: String::from(path),
            kind: Kind::Dir,
            mode: 0o755,
            data: &[],
        });
    }

    /// What is at an absolute path.
    pub fn get(&self, path: &str) -> Option<&Node<'a>> {
        self.nodes
            .binary_search_by(|node| node.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.nodes[index])
    }

    /// What is directly in a directory, by name.
    pub fn children<'s>(&'s self, dir: &'s str) -> impl Iterator<Item = &'s Node<'a>> + 's {
        self.nodes
            .iter()
            .filter(move |node| node.path != "/" && parent_of(&node.path) == Some(dir))
    }

    /// Everything under a directory, at any depth, in path order.
    pub fn under<'s>(&'s self, dir: &'s str) -> impl Iterator<Item = &'s Node<'a>> + 's {
        self.nodes
            .iter()
            .filter(move |node| is_inside(&node.path, dir))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }
}

/// Whether `path` is strictly inside `dir`.
fn is_inside(path: &str, dir: &str) -> bool {
    if dir == "/" {
        return path != "/";
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// A header field: bytes up to the first NUL.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// A header number: octal digits, perhaps padded with spaces and NULs.
fn octal(field: &[u8]) -> Result<u64, Error> {
    let mut value: u64 = 0;
    let mut seen = false;
    for b in field {
        match b {
            b'0'..=b'7' => {
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add((b - b'0') as u64))
                    .ok_or(Error::Number)?;
                seen = true;
            }
            b' ' | 0 if !seen => {}
            b' ' | 0 => break,
            _ => return Err(Error::Number),
        }
    }
    Ok(value)
}

/// The directory a path is in, or `None` for `/`.
pub fn parent_of(path: &str) -> Option<&str> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/"),
        Some(at) => Some(&path[..at]),
        None => None,
    }
}

/// The last part of a path.
pub fn name_of(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// `path` as an absolute path with no `.`, `..` or doubled slashes, taken
/// from `cwd` if it is relative. `..` at the top stays at the top.
pub fn resolve(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let start = if path.starts_with('/') { "" } else { cwd };
    for part in start.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Whether `name` matches a pattern of `*` (any run), `?` (any one) and
/// characters that stand for themselves.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    // Where the last `*` was, and how much of the name it had taken.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ni += 1;
            }
            Some(c) if *c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((at, taken)) => {
                    pi = at + 1;
                    ni = taken + 1;
                    star = Some((at, taken + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// `-rw-r--r--` for a file, `drwxr-xr-x` for a directory.
pub fn mode_string(kind: Kind, mode: u32) -> String {
    let mut out = String::from(if kind == Kind::Dir { "d" } else { "-" });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}
