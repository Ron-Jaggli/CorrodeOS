//! msvcrt's streams for reading files: `fopen` and what reads through it.
//!
//! A program sees files only through its view, and a view can't be
//! written, so a stream here is read-only: `fopen` with `w`, `a` or `+`
//! fails with `EACCES`, as Windows' runtime fails on a read-only file.
//! Each open stream is a `FILE` msvcrt's way (the program may look inside
//! one, as mingw's headers allow) with a buffer behind it, filled from the
//! view a buffer at a time.
//!
//! A file the view hides fails exactly as one that doesn't exist does:
//! `ENOENT` and the same text, so a program can't tell them apart.

use crate::msvcrt::{set_errno, stream, File, EBADF, EINVAL, EOF};
use crate::{close, open_for_reading, read_some, One};

const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EMFILE: i32 = 24;
const ENAMETOOLONG: i32 = 38;

/// As many streams as the view has handles.
const STREAMS: usize = corrode_rt::abi::view::HANDLES;
const BUFFER: usize = 512;

/// An open stream. `file` comes first, so a pointer to the stream is a
/// pointer to its `FILE`.
#[repr(C)]
struct Stream {
    file: File,
    handle: isize,
    /// Text mode: a carriage return before a line feed is dropped.
    text: bool,
    at: usize,
    len: usize,
    end: bool,
    error: bool,
    buffer: [u8; BUFFER],
}

static STREAMS_OPEN: One<[Option<Stream>; STREAMS]> = One::new([const { None }; STREAMS]);

/// The stream `file` points at, if it is an open one this runtime gave.
fn stream_of(file: *mut File) -> Option<&'static mut Stream> {
    // SAFETY: one thread, and no reference outlives the call that took it.
    let streams = unsafe { STREAMS_OPEN.get() };
    streams.iter_mut().flatten().find(|stream| core::ptr::eq(&stream.file, file))
}

/// A Windows error from the view as the C runtime's error number.
fn errno_of(code: u32) -> i32 {
    match code {
        2 | 3 => ENOENT,
        5 => EACCES,
        4 => EMFILE,
        206 => ENAMETOOLONG,
        _ => EINVAL,
    }
}

/// # Safety
/// `name` and `mode` are zero-ended strings.
pub unsafe extern "win64" fn fopen(name: *const u8, mode: *const u8) -> *mut File {
    if name.is_null() || mode.is_null() {
        set_errno(EINVAL);
        return core::ptr::null_mut();
    }
    // SAFETY: the caller's promise.
    let (name, mode) = unsafe {
        (core::ffi::CStr::from_ptr(name.cast()).to_bytes(), core::ffi::CStr::from_ptr(mode.cast()).to_bytes())
    };
    match mode.first() {
        Some(b'r') if !mode.contains(&b'+') => {}
        Some(b'r' | b'w' | b'a') => {
            set_errno(EACCES);
            return core::ptr::null_mut();
        }
        _ => {
            set_errno(EINVAL);
            return core::ptr::null_mut();
        }
    }
    // SAFETY: one thread.
    let streams = unsafe { STREAMS_OPEN.get() };
    let Some(slot) = streams.iter_mut().find(|slot| slot.is_none()) else {
        set_errno(EMFILE);
        return core::ptr::null_mut();
    };
    let handle = match open_for_reading(name) {
        Ok(handle) => handle,
        Err(code) => {
            set_errno(errno_of(code));
            return core::ptr::null_mut();
        }
    };
    let opened = slot.insert(Stream {
        file: stream(handle as i32),
        handle,
        text: !mode.contains(&b'b'),
        at: 0,
        len: 0,
        end: false,
        error: false,
        buffer: [0; BUFFER],
    });
    &mut opened.file
}

pub extern "win64" fn fclose(file: *mut File) -> i32 {
    // SAFETY: one thread.
    let streams = unsafe { STREAMS_OPEN.get() };
    match streams.iter_mut().find(|slot| slot.as_ref().is_some_and(|stream| core::ptr::eq(&stream.file, file))) {
        Some(slot) => {
            if let Some(stream) = slot.take() {
                close(stream.handle);
            }
            0
        }
        // The standard streams close as nothing; anything else is not a
        // stream.
        None if crate::msvcrt::is_standard(file) => 0,
        None => {
            set_errno(EBADF);
            EOF
        }
    }
}

impl Stream {
    /// The next byte, before text mode's translation.
    fn raw(&mut self) -> Option<u8> {
        if self.at == self.len {
            if self.end || self.error {
                return None;
            }
            match read_some(self.handle, &mut self.buffer) {
                Ok(0) => {
                    self.end = true;
                    return None;
                }
                Ok(got) => (self.at, self.len) = (0, got),
                Err(_) => {
                    self.error = true;
                    return None;
                }
            }
        }
        self.at += 1;
        Some(self.buffer[self.at - 1])
    }

    /// The next byte as the program reads it.
    fn next(&mut self) -> Option<u8> {
        let byte = self.raw()?;
        if self.text && byte == b'\r' {
            // Look one ahead: kept unless a line feed follows.
            if self.at == self.len {
                if let Some(after) = self.raw() {
                    if after == b'\n' {
                        return Some(b'\n');
                    }
                    self.at -= 1;
                }
            } else if self.buffer[self.at] == b'\n' {
                self.at += 1;
                return Some(b'\n');
            }
        }
        Some(byte)
    }
}

pub extern "win64" fn fgetc(file: *mut File) -> i32 {
    match stream_of(file) {
        Some(stream) => stream.next().map_or(EOF, i32::from),
        None => {
            set_errno(EBADF);
            EOF
        }
    }
}

/// # Safety
/// `into` has room for `len` bytes.
pub unsafe extern "win64" fn fgets(into: *mut u8, len: i32, file: *mut File) -> *mut u8 {
    let Some(stream) = stream_of(file) else {
        set_errno(EBADF);
        return core::ptr::null_mut();
    };
    if into.is_null() || len <= 0 {
        set_errno(EINVAL);
        return core::ptr::null_mut();
    }
    let mut count = 0usize;
    while count + 1 < len as usize {
        let Some(byte) = stream.next() else { break };
        // SAFETY: the caller's promise; `count < len - 1`.
        unsafe { into.add(count).write(byte) };
        count += 1;
        if byte == b'\n' {
            break;
        }
    }
    if count == 0 && len > 1 {
        return core::ptr::null_mut();
    }
    // SAFETY: `count < len`.
    unsafe { into.add(count).write(0) };
    into
}

/// # Safety
/// `into` has room for `size * count` bytes.
pub unsafe extern "win64" fn fread(into: *mut u8, size: usize, count: usize, file: *mut File) -> usize {
    let Some(stream) = stream_of(file) else {
        set_errno(EBADF);
        return 0;
    };
    let Some(want) = size.checked_mul(count) else {
        set_errno(EINVAL);
        return 0;
    };
    if want == 0 {
        return 0;
    }
    let mut got = 0;
    while got < want {
        let Some(byte) = stream.next() else { break };
        // SAFETY: the caller's promise; `got < want`.
        unsafe { into.add(got).write(byte) };
        got += 1;
    }
    got / size
}

pub extern "win64" fn feof(file: *mut File) -> i32 {
    stream_of(file).map_or(0, |stream| i32::from(stream.end && stream.at == stream.len))
}

pub extern "win64" fn ferror(file: *mut File) -> i32 {
    stream_of(file).map_or(0, |stream| i32::from(stream.error))
}
