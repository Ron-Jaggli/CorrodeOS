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
    CLOSE, DONE, HANDLES, HEAP, HEAP_PAGES, INVALID_HANDLE, LOG, MOST_PATH, NAME_TOO_LONG, OPEN, PAGE, POOL, READ,
    REPLY, SERVE, SIZE, SPEC, TOO_MANY_OPEN, TREE,
};
use corrode_rt::{identify, map, println, recv, reply, Message};
use corrode_shell::fs::{Fs, Node};
use corrode_view::View;

corrode_rt::entry!(main);

const PAGE_LEN: usize = 4096;

/// An open file: what it is, and how far it has been read.
struct Open<'f, 'a> {
    node: &'f Node<'a>,
    at: usize,
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
        let answer: Result<u64, u64> = match label {
            OPEN => {
                let len = words[0] as usize;
                if len > MOST_PATH.min(PAGE_LEN) {
                    Err(NAME_TOO_LONG)
                } else {
                    // Copied first: the page is the program's to change.
                    let copied = Vec::from(&page[..len]);
                    match core::str::from_utf8(&copied) {
                        Err(_) => Err(corrode_view::Error::FileNotFound.code() as u64),
                        Ok(path) => match view.open(&fs, view.home(), path, words[1] != 0, words[2] as u32) {
                            Err(error) => Err(error.code() as u64),
                            Ok(node) => match open.iter().position(Option::is_none) {
                                None => Err(TOO_MANY_OPEN),
                                Some(handle) => {
                                    open[handle] = Some(Open { node, at: 0 });
                                    Ok(handle as u64)
                                }
                            },
                        },
                    }
                }
            }
            READ | SIZE | CLOSE => match open.get_mut(words[0] as usize).and_then(Option::as_mut) {
                None => Err(INVALID_HANDLE),
                Some(file) if label == READ => {
                    let from = file.at.min(file.node.data.len());
                    let take = (words[1] as usize).min(PAGE_LEN).min(file.node.data.len() - from);
                    page[..take].copy_from_slice(&file.node.data[from..from + take]);
                    file.at = from + take;
                    Ok(take as u64)
                }
                Some(file) if label == SIZE => Ok(file.node.data.len() as u64),
                Some(_) => {
                    open[words[0] as usize] = None;
                    Ok(0)
                }
            },
            _ => Err(INVALID_HANDLE),
        };
        let words = match answer {
            Ok(value) => [0, value, 0, 0],
            Err(error) => [error, 0, 0, 0],
        };
        if identify(REPLY).is_ok() {
            let _ = reply(REPLY, Message::new(label, words));
        }
    }
}
