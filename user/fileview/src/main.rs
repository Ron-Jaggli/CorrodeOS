//! A Windows program's view of the files (`verdigris/DESIGN.md` section 4).
//!
//! One of these runs for each Windows program given files. It holds what
//! the program must not: the real tree (the archive, read-only) and the
//! list of what the program may see, its own folder first. The program
//! holds only the view's endpoint, send-only, and a page shared with this
//! task for paths and what is read.
//!
//! Every path is resolved here, against the list, by `corrode-view`: what
//! is not allowed is not there, exactly as a name that does not exist. A
//! path is copied out of the shared page before it is looked at, so the
//! program changing the page meanwhile changes nothing that was checked.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use corrode_rt::abi::view::{
    CLOSE, DONE, HANDLES, HEAP, HEAP_PAGES, INVALID_HANDLE, LOG, MOST_PATH, NAME_TOO_LONG, NEXT, NO_MORE_FILES, OPEN,
    PAGE, POOL, READ, REPLY, SEARCH, SERVE, SIZE, SPEC, TOO_MANY_OPEN, TREE, GET_DIR, SET_DIR, FULL_PATH,
};
use corrode_rt::{identify, map, println, recv, reply, Message};
use corrode_shell::fs::{Fs, Node};
use corrode_view::{Found, View};

corrode_rt::entry!(main);

const PAGE_LEN: usize = 4096;

/// A handle: an open file and how far it has been read, or what a search
/// found and how many of those were handed out.
enum Open<'f, 'a> {
    File { node: &'f Node<'a>, at: usize },
    Search { found: Vec<Found>, at: usize },
}

/// A path the program put in the page, copied out first: the page is the
/// program's to change.
fn path_in(page: &[u8], len: u64) -> Result<String, u64> {
    let len = len as usize;
    if len > MOST_PATH.min(PAGE_LEN) {
        return Err(NAME_TOO_LONG);
    }
    let copied = Vec::from(&page[..len]);
    String::from_utf8(copied).map_err(|_| corrode_view::Error::FileNotFound.code() as u64)
}

/// `text` into the page, answering its length.
fn to_page(page: &mut [u8], text: &str) -> [u64; 3] {
    let len = text.len().min(page.len());
    page[..len].copy_from_slice(&text.as_bytes()[..len]);
    [len as u64, 0, 0]
}

/// The first free handle, holding `open`.
fn keep<'f, 'a>(handles: &mut [Option<Open<'f, 'a>>], open: Open<'f, 'a>) -> Result<[u64; 3], u64> {
    let handle = handles.iter().position(Option::is_none).ok_or(TOO_MANY_OPEN)?;
    handles[handle] = Some(open);
    Ok([handle as u64, 0, 0])
}

/// Map a capability's memory as bytes, or say why not.
fn bytes(slot: usize, what: &str) -> Option<&'static mut [u8]> {
    match map(slot) {
        // SAFETY: the kernel mapped `len` bytes here for the life of this
        // task. The tree and the list are read-only to every holder; the
        // page is shared with the program, and is only ever copied from or
        // into, never trusted in place.
        Ok(mapping) => Some(unsafe { core::slice::from_raw_parts_mut(mapping.address, mapping.len) }),
        Err(error) => {
            println!("no {what}: {error:?}");
            None
        }
    }
}

/// The list: its own folder, then the allowed paths, a line each.
fn spec(bytes: &[u8]) -> Option<(String, Vec<String>)> {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let text = core::str::from_utf8(&bytes[..end]).ok()?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty()).map(String::from);
    let folder = lines.next()?;
    Some((folder, lines.collect()))
}

fn main() {
    corrode_rt::log::set(LOG);
    if corrode_rt::heap::init(POOL, HEAP, HEAP_PAGES).is_err() {
        println!("no heap");
        return;
    }
    let (Some(tree), Some(page), Some(list)) = (bytes(TREE, "files"), bytes(PAGE, "page"), bytes(SPEC, "list")) else {
        return;
    };
    let fs = match Fs::from_tar(tree) {
        Ok(fs) => fs,
        Err(error) => {
            println!("the files are not an archive: {error:?}");
            return;
        }
    };
    let Some((folder, allowed)) = spec(list) else {
        println!("no list of what the program may see");
        return;
    };
    let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
    let view = View::new(&folder, &allowed);
    println!("C:\\ is {}, {} allowed beside {folder}", view.root(), allowed.len());

    // Relative paths start here: the program's own folder, until it says.
    let mut cwd = String::from(view.home());
    let mut open: [Option<Open>; HANDLES] = [const { None }; HANDLES];
    loop {
        let received = match recv(SERVE, None, Some(REPLY)) {
            Ok(received) => received,
            Err(error) => {
                println!("cannot receive: {error:?}");
                return;
            }
        };
        let label = received.message.label;
        let words = received.message.words;
        if label == DONE {
            return;
        }
        let answer: Result<[u64; 3], u64> = match label {
            OPEN => path_in(page, words[0]).and_then(|path| {
                let node = view
                    .open(&fs, &cwd, &path, words[1] != 0, words[2] as u32)
                    .map_err(|error| error.code() as u64)?;
                keep(&mut open, Open::File { node, at: 0 })
            }),
            SEARCH => path_in(page, words[0]).and_then(|path| {
                let found = view.search(&fs, &cwd, &path).map_err(|error| error.code() as u64)?;
                keep(&mut open, Open::Search { found, at: 0 })
            }),
            SET_DIR => path_in(page, words[0]).and_then(|path| {
                cwd = view.change_dir(&fs, &cwd, &path).map_err(|error| error.code() as u64)?;
                Ok([0; 3])
            }),
            GET_DIR => Ok(to_page(page, &view.windows_path(&cwd))),
            FULL_PATH => path_in(page, words[0]).and_then(|path| {
                let full = view.full_path(&cwd, &path).map_err(|error| error.code() as u64)?;
                Ok(to_page(page, &full))
            }),
            READ | SIZE | NEXT | CLOSE => match open.get_mut(words[0] as usize).and_then(Option::as_mut) {
                None => Err(INVALID_HANDLE),
                Some(_) if label == CLOSE => {
                    open[words[0] as usize] = None;
                    Ok([0; 3])
                }
                Some(Open::File { node, at }) if label == READ => {
                    let from = (*at).min(node.data.len());
                    let take = (words[1] as usize).min(PAGE_LEN).min(node.data.len() - from);
                    page[..take].copy_from_slice(&node.data[from..from + take]);
                    *at = from + take;
                    Ok([take as u64, 0, 0])
                }
                Some(Open::File { node, .. }) if label == SIZE => Ok([node.data.len() as u64, 0, 0]),
                Some(Open::Search { found, at }) if label == NEXT => match found.get(*at) {
                    None => Err(NO_MORE_FILES),
                    Some(entry) => {
                        let name = entry.name.as_bytes();
                        let len = name.len().min(PAGE_LEN);
                        page[..len].copy_from_slice(&name[..len]);
                        *at += 1;
                        Ok([len as u64, entry.size, u64::from(entry.dir)])
                    }
                },
                Some(_) => Err(INVALID_HANDLE),
            },
            _ => Err(INVALID_HANDLE),
        };
        let words = match answer {
            Ok([a, b, c]) => [0, a, b, c],
            Err(error) => [error, 0, 0, 0],
        };
        if identify(REPLY).is_ok() {
            let _ = reply(REPLY, Message::new(label, words));
        }
    }
}
