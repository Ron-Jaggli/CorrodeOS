//! Verdigris's kernel32: the smallest part of it a console program needs.
//!
//! `GetStdHandle`, `WriteFile` and `ExitProcess`, called by Windows code
//! with the Windows calling convention, in the Windows program's own task.
//! Standard output and error are the log endpoint, the program's first
//! capability (`corrode_abi::verdigris::LOG`): what a program writes comes
//! out as log lines under its name.
//!
//! Files: `CreateFileW`, `ReadFile`, `GetFileSize`, `CloseHandle`, and the
//! search calls `FindFirstFileW`, `FindNextFileW` and `FindClose`, and the
//! current directory and full paths (`GetCurrentDirectoryW`,
//! `SetCurrentDirectoryW`, `GetFullPathNameW`, always `C:\` paths), through
//! the program's view (`verdigris::VIEW`), when it was given one. Paths go
//! to the view's service in the page the two share (`verdigris::PAGE`),
//! and what is read comes back in it; the service decides what the program
//! may see, and this file never does. Failures set `GetLastError`'s
//! number, as on Windows. Nothing here holds or asks for more.
//!
//! `GetCommandLineW`: the kernel leaves the command line at the start of
//! the shared page before the program starts, and the first use of the
//! page takes it from there.
//!
//! This is not a PE DLL yet (see `verdigris/DESIGN.md`, section 2): it is a
//! CorrodeOS program linked at `KERNEL32_BASE` by `kernel32.ld`, carrying
//! its export table at that address, which the kernel reads to bind a
//! program's imports. It never runs on its own; its entry point exists only
//! because a program has to have one.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use corrode_pe::exports::{DLL_LEN, ENTRY, MAGIC, NAME_LEN};
use corrode_rt::abi::log::{CAPACITY, LINE, TEXT};
use corrode_rt::abi::verdigris::{LOG, PAGE, VIEW};
use corrode_rt::abi::view;
use corrode_rt::{call, map, send, Message};

corrode_rt::entry!(main);

/// Never called: the kernel starts a Windows program at its own entry.
fn main() {}

/// What `GetStdHandle` hands out: not capabilities, just numbers this
/// kernel32 recognises.
const STDIN: isize = 0x10;
const STDOUT: isize = 0x11;
const STDERR: isize = 0x12;
const INVALID_HANDLE_VALUE: isize = -1;
/// A file's handle is this plus the view's number for it.
const FILE_HANDLES: isize = 0x100;
/// A search's handle is this plus the view's number for it.
const SEARCH_HANDLES: isize = 0x200;

/// `GetLastError`'s numbers that this file gives itself.
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_FILENAME_EXCED_RANGE: u32 = 206;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// Rights to change a file, any of which makes an open a write.
const WRITE_RIGHTS: u32 = 0x4000_0000 // GENERIC_WRITE
    | 0x1000_0000 // GENERIC_ALL
    | 0x0001_0000 // DELETE
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0004 // FILE_APPEND_DATA
    | 0x0000_0002; // FILE_WRITE_DATA

/// `GetLastError`'s number: one thread, so one number.
static LAST_ERROR: AtomicU32 = AtomicU32::new(0);

extern "win64" fn get_last_error() -> u32 {
    LAST_ERROR.load(Ordering::Relaxed)
}

extern "win64" fn set_last_error(code: u32) {
    LAST_ERROR.store(code, Ordering::Relaxed);
}

/// Where the shared page is mapped, once it is: 0 until then.
static PAGE_AT: AtomicUsize = AtomicUsize::new(0);

/// The command line: UTF-16, ending in a zero.
struct CommandLine(UnsafeCell<[u16; 2048]>);

// SAFETY: one thread (see `Pending`).
unsafe impl Sync for CommandLine {}

static COMMAND_LINE: CommandLine = CommandLine(UnsafeCell::new([0; 2048]));

/// The page shared with the view, mapped the first time it is needed. Its
/// first use takes the command line out of it, before anything is written.
fn page() -> Option<&'static mut [u8; 4096]> {
    let mut at = PAGE_AT.load(Ordering::Relaxed);
    if at == 0 {
        let mapping = map(PAGE).ok().filter(|mapping| mapping.len >= 4096)?;
        at = mapping.address as usize;
        PAGE_AT.store(at, Ordering::Relaxed);
        // SAFETY: as below, and nothing else holds the command line now.
        let (shared, line) = unsafe { (&*(at as *const [u8; 4096]), &mut *COMMAND_LINE.0.get()) };
        // The last unit stays zero, whatever the page holds.
        for (unit, bytes) in line[..2047].iter_mut().zip(shared.as_chunks::<2>().0) {
            *unit = u16::from_le_bytes(*bytes);
        }
    }
    // SAFETY: the kernel mapped at least a page here, writable, for the life
    // of this task; one thread, and no reference to it outlives a call.
    Some(unsafe { &mut *(at as *mut [u8; 4096]) })
}

/// Ask the view. `Err` is a Windows error number: the view's, or "not
/// found" for a program that was given no view (it has no files at all).
fn ask(label: u64, words: [u64; 4]) -> Result<u64, u32> {
    match call(VIEW, Message::new(label, words), None) {
        Ok(answer) if answer.message.words[0] == 0 => Ok(answer.message.words[1]),
        Ok(answer) => Err(answer.message.words[0] as u32),
        Err(_) => Err(ERROR_FILE_NOT_FOUND),
    }
}

/// The view's number for a handle in the range from `first`, if it is one.
fn in_range(handle: isize, first: isize) -> Option<u64> {
    (first..first + view::HANDLES as isize)
        .contains(&handle)
        .then(|| (handle - first) as u64)
}

/// The view's number for a file handle, if it is one.
fn file_of(handle: isize) -> Option<u64> {
    in_range(handle, FILE_HANDLES)
}

/// The view's number for a search handle, if it is one.
fn search_of(handle: isize) -> Option<u64> {
    in_range(handle, SEARCH_HANDLES)
}

/// A zero-ended UTF-16 path, as UTF-8 in the shared page: its length, or
/// the Windows error. An unpaired surrogate is U+FFFD.
///
/// # Safety
/// `name` is null or points at a zero-ended UTF-16 string.
unsafe fn path_to_page(name: *const u16) -> Result<usize, u32> {
    if name.is_null() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    let page = page().ok_or(ERROR_FILE_NOT_FOUND)?;
    let units = (0..).map(|i| {
        // SAFETY: the caller's promise; read only up to the zero.
        unsafe { name.add(i).read_unaligned() }
    });
    let mut len = 0;
    for character in char::decode_utf16(units.take_while(|unit| *unit != 0)) {
        let character = character.unwrap_or(char::REPLACEMENT_CHARACTER);
        if len + character.len_utf8() > view::MOST_PATH {
            return Err(ERROR_FILENAME_EXCED_RANGE);
        }
        len += character.encode_utf8(&mut page[len..]).len();
    }
    Ok(len)
}

/// Fail with `code`, as a Windows call does: the number for
/// `GetLastError`, and `value` returned.
fn fail<T>(code: u32, value: T) -> T {
    set_last_error(code);
    value
}

/// Open a file through the view. The name is UTF-16, ended by a zero.
///
/// # Safety
/// `name` points at a zero-ended UTF-16 string, as the Windows contract
/// says. A program that lies faults in its own task.
unsafe extern "win64" fn create_file_w(
    name: *const u16,
    access: u32,
    _share: u32,
    _security: *mut (),
    disposition: u32,
    _flags: u32,
    _template: isize,
) -> isize {
    // SAFETY: the caller's promise.
    let len = match unsafe { path_to_page(name) } {
        Ok(len) => len,
        Err(code) => return fail(code, INVALID_HANDLE_VALUE),
    };
    let write = u64::from(access & WRITE_RIGHTS != 0);
    match ask(view::OPEN, [len as u64, write, disposition as u64, 0]) {
        Ok(file) => {
            set_last_error(0);
            FILE_HANDLES + file as isize
        }
        Err(code) => fail(code, INVALID_HANDLE_VALUE),
    }
}

/// Read up to `len` bytes of an open file into `buffer`, a page at a time
/// through the view. Reading standard input is not done yet.
///
/// # Safety
/// `buffer` has room for `len` bytes and `read` is null or writable, as
/// the Windows contract says.
unsafe extern "win64" fn read_file(
    handle: isize,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    _overlapped: *mut (),
) -> i32 {
    if !read.is_null() {
        // SAFETY: the caller's promise.
        unsafe { read.write_unaligned(0) };
    }
    let (Some(file), Some(page)) = (file_of(handle), page()) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    let mut done = 0usize;
    while done < len as usize {
        let want = (len as usize - done).min(page.len());
        let got = match ask(view::READ, [file, want as u64, 0, 0]) {
            Ok(got) => (got as usize).min(want),
            Err(code) => return fail(code, 0),
        };
        // SAFETY: the caller's promise; `done + got <= len`.
        unsafe { core::ptr::copy_nonoverlapping(page.as_ptr(), buffer.add(done), got) };
        done += got;
        if got < want {
            break;
        }
    }
    if !read.is_null() {
        // SAFETY: the caller's promise.
        unsafe { read.write_unaligned(done as u32) };
    }
    1
}

/// `INVALID_FILE_SIZE`, which is also a size: `GetLastError` tells which.
const INVALID_FILE_SIZE: u32 = u32::MAX;

/// An open file's size: the low 32 bits returned, the high ones in `high`.
///
/// # Safety
/// `high` is null or writable.
unsafe extern "win64" fn get_file_size(handle: isize, high: *mut u32) -> u32 {
    let Some(file) = file_of(handle) else {
        return fail(ERROR_INVALID_HANDLE, INVALID_FILE_SIZE);
    };
    match ask(view::SIZE, [file, 0, 0, 0]) {
        Ok(size) => {
            if !high.is_null() {
                // SAFETY: the caller's promise.
                unsafe { high.write_unaligned((size >> 32) as u32) };
            }
            set_last_error(0);
            size as u32
        }
        Err(code) => fail(code, INVALID_FILE_SIZE),
    }
}

/// A `C:\` path the view put in the page, `len` bytes of UTF-8, into a
/// program's buffer of `room` UTF-16 units, as Windows' string calls do:
/// the units written, without the zero; or, if it does not fit with its
/// zero, the room it needs, zero included, and nothing written. With it,
/// the unit after the last `\`, for `GetFullPathNameW`'s file part.
///
/// # Safety
/// `buffer` is null or has room for `room` units.
unsafe fn page_to_buffer(len: u64, room: u32, buffer: *mut u16) -> (u32, Option<usize>) {
    let Some(page) = page() else {
        return (0, None);
    };
    let text = core::str::from_utf8(&page[..(len as usize).min(page.len())]).unwrap_or("");
    let units = text.encode_utf16().count();
    if buffer.is_null() || units + 1 > room as usize {
        return (units as u32 + 1, None);
    }
    let mut after_slash = None;
    for (at, unit) in text.encode_utf16().enumerate() {
        // SAFETY: the caller's promise; `at < units < room`.
        unsafe { buffer.add(at).write_unaligned(unit) };
        if unit == u16::from(b'\\') {
            after_slash = Some(at + 1);
        }
    }
    // SAFETY: as above; `units < room`.
    unsafe { buffer.add(units).write_unaligned(0) };
    (units as u32, after_slash.filter(|at| *at < units))
}

/// The current directory, a `C:\` path, into `buffer` (see
/// [`page_to_buffer`] for what is returned). 0 if it cannot be had.
///
/// # Safety
/// `buffer` is null or has room for `room` units.
unsafe extern "win64" fn get_current_directory_w(room: u32, buffer: *mut u16) -> u32 {
    match ask(view::GET_DIR, [0; 4]) {
        // SAFETY: the caller's promise.
        Ok(len) => unsafe { page_to_buffer(len, room, buffer) }.0,
        Err(code) => fail(code, 0),
    }
}

/// Make `path` the current directory. Only a directory the view shows.
///
/// # Safety
/// `path` is a zero-ended UTF-16 string.
unsafe extern "win64" fn set_current_directory_w(path: *const u16) -> i32 {
    // SAFETY: the caller's promise.
    let len = match unsafe { path_to_page(path) } {
        Ok(len) => len,
        Err(code) => return fail(code, 0),
    };
    match ask(view::SET_DIR, [len as u64, 0, 0, 0]) {
        Ok(_) => 1,
        Err(code) => fail(code, 0),
    }
}

/// `name` made absolute and clean, a `C:\` path, into `buffer`; and, if
/// `file_part` is not null, where in it the last name starts (null if it
/// ends with `\`). Nothing is asked of the files.
///
/// # Safety
/// `name` is a zero-ended UTF-16 string, `buffer` null or with room for
/// `room` units, `file_part` null or writable.
unsafe extern "win64" fn get_full_path_name_w(
    name: *const u16,
    room: u32,
    buffer: *mut u16,
    file_part: *mut *mut u16,
) -> u32 {
    // SAFETY: the caller's promise.
    let len = match unsafe { path_to_page(name) } {
        Ok(len) => len,
        Err(code) => return fail(code, 0),
    };
    let full = match ask(view::FULL_PATH, [len as u64, 0, 0, 0]) {
        Ok(full) => full,
        Err(code) => return fail(code, 0),
    };
    // SAFETY: the caller's promise.
    let (count, last) = unsafe { page_to_buffer(full, room, buffer) };
    if !file_part.is_null() {
        let part = match last {
            // SAFETY: inside what was just written.
            Some(at) if count < room => unsafe { buffer.add(at) },
            _ => core::ptr::null_mut(),
        };
        // SAFETY: the caller's promise.
        unsafe { file_part.write_unaligned(part) };
    }
    count
}

/// `WIN32_FIND_DATAW`'s fields, by byte offset.
const FIND_ATTRIBUTES: usize = 0;
const FIND_SIZE_HIGH: usize = 28;
const FIND_SIZE_LOW: usize = 32;
const FIND_NAME: usize = 44;
const FIND_NAME_UNITS: usize = 260;
const FIND_DATA_LEN: usize = 592;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// The next entry of a search, into `data`: false, with the error set, if
/// there is none.
///
/// # Safety
/// `data` is a writable `WIN32_FIND_DATAW`.
unsafe fn next_found(search: u64, data: *mut u8) -> bool {
    let Some(page) = page() else {
        return fail(ERROR_INVALID_HANDLE, false);
    };
    let answer = match call(VIEW, Message::new(view::NEXT, [search, 0, 0, 0]), None) {
        Ok(answer) if answer.message.words[0] == 0 => answer.message.words,
        Ok(answer) => return fail(answer.message.words[0] as u32, false),
        Err(_) => return fail(ERROR_INVALID_HANDLE, false),
    };
    let [_, len, size, dir] = answer;
    let mut record = [0u8; FIND_DATA_LEN];
    // Nothing in the view can be written: files say so.
    let attributes = if dir == 1 { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_READONLY };
    record[FIND_ATTRIBUTES..FIND_ATTRIBUTES + 4].copy_from_slice(&attributes.to_le_bytes());
    record[FIND_SIZE_HIGH..FIND_SIZE_HIGH + 4].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    record[FIND_SIZE_LOW..FIND_SIZE_LOW + 4].copy_from_slice(&(size as u32).to_le_bytes());
    // The name, UTF-8 to UTF-16, cut to fit with its zero.
    let name = core::str::from_utf8(&page[..(len as usize).min(page.len())]).unwrap_or("?");
    let mut at = FIND_NAME;
    for unit in name.encode_utf16().take(FIND_NAME_UNITS - 1) {
        record[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        at += 2;
    }
    // SAFETY: the caller's promise.
    unsafe { core::ptr::copy_nonoverlapping(record.as_ptr(), data, FIND_DATA_LEN) };
    true
}

/// Search a directory: the last name of `name` is the pattern. The first
/// entry goes into `data`, and the handle gives the rest.
///
/// # Safety
/// `name` is a zero-ended UTF-16 string and `data` a writable
/// `WIN32_FIND_DATAW`, as the Windows contract says.
unsafe extern "win64" fn find_first_file_w(name: *const u16, data: *mut u8) -> isize {
    if data.is_null() {
        return fail(ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE);
    }
    // SAFETY: the caller's promise.
    let len = match unsafe { path_to_page(name) } {
        Ok(len) => len,
        Err(code) => return fail(code, INVALID_HANDLE_VALUE),
    };
    let search = match ask(view::SEARCH, [len as u64, 0, 0, 0]) {
        Ok(search) => search,
        Err(code) => return fail(code, INVALID_HANDLE_VALUE),
    };
    // SAFETY: the caller's promise. A search finds at least one entry.
    if !unsafe { next_found(search, data) } {
        let _ = ask(view::CLOSE, [search, 0, 0, 0]);
        return INVALID_HANDLE_VALUE;
    }
    set_last_error(0);
    SEARCH_HANDLES + search as isize
}

/// The next entry of a search. False with `ERROR_NO_MORE_FILES` after the
/// last.
///
/// # Safety
/// `data` is a writable `WIN32_FIND_DATAW`.
unsafe extern "win64" fn find_next_file_w(handle: isize, data: *mut u8) -> i32 {
    let Some(search) = search_of(handle) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    if data.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    // SAFETY: the caller's promise.
    i32::from(unsafe { next_found(search, data) })
}

/// End a search.
extern "win64" fn find_close(handle: isize) -> i32 {
    let Some(search) = search_of(handle) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    match ask(view::CLOSE, [search, 0, 0, 0]) {
        Ok(_) => 1,
        Err(code) => fail(code, 0),
    }
}

/// Close a handle. Standard handles close as nothing.
extern "win64" fn close_handle(handle: isize) -> i32 {
    if matches!(handle, STDIN | STDOUT | STDERR) {
        return 1;
    }
    let Some(file) = file_of(handle) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    match ask(view::CLOSE, [file, 0, 0, 0]) {
        Ok(_) => 1,
        Err(code) => fail(code, 0),
    }
}

/// `STD_INPUT_HANDLE` and the rest, as the `u32` they arrive as.
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

/// The command line, as the program was started with it. Empty for a
/// program given no view.
extern "win64" fn get_command_line_w() -> *const u16 {
    let _ = page();
    COMMAND_LINE.0.get().cast::<u16>()
}

/// The handle for standard input, output or error.
extern "win64" fn get_std_handle(which: u32) -> isize {
    match which {
        STD_INPUT_HANDLE => STDIN,
        STD_OUTPUT_HANDLE => STDOUT,
        STD_ERROR_HANDLE => STDERR,
        _ => INVALID_HANDLE_VALUE,
    }
}

/// The part of a line written so far, waiting for its end.
struct Pending {
    bytes: UnsafeCell<[u8; CAPACITY]>,
    len: UnsafeCell<usize>,
}

// SAFETY: a Windows program has one thread here (CorrodeOS has no ring 3
// threads), so nothing ever reaches this from two places at once.
unsafe impl Sync for Pending {}

static PENDING: Pending = Pending {
    bytes: UnsafeCell::new([0; CAPACITY]),
    len: UnsafeCell::new(0),
};

impl Pending {
    /// Send what is waiting, as more of the line or as its end.
    fn flush(&self, label: u64) {
        // SAFETY: one thread (see above), and no reference outlives this.
        let (bytes, len) = unsafe { (&mut *self.bytes.get(), &mut *self.len.get()) };
        let mut words = [0u64; 4];
        for (i, byte) in bytes[..*len].iter().enumerate() {
            words[i / 8] |= (*byte as u64) << ((i % 8) * 8);
        }
        // A log that refuses the line is not worth failing the write for.
        let _ = send(LOG, Message::new(label, words));
        *len = 0;
    }

    /// Take one byte of output. A line feed ends a line; a carriage return,
    /// half of Windows' line ending, is dropped; zero would end a message's
    /// text early, so it is shown as a space.
    fn push(&self, byte: u8) {
        if byte == b'\n' {
            self.flush(LINE);
            return;
        }
        if byte == b'\r' {
            return;
        }
        // SAFETY: as in `flush`.
        let full = unsafe { *self.len.get() == CAPACITY };
        if full {
            self.flush(TEXT);
        }
        // SAFETY: as in `flush`; `len` is below CAPACITY here.
        unsafe {
            let len = &mut *self.len.get();
            (*self.bytes.get())[*len] = if byte == 0 { b' ' } else { byte };
            *len += 1;
        }
    }

    fn waiting(&self) -> bool {
        // SAFETY: as in `flush`.
        unsafe { *self.len.get() != 0 }
    }
}

/// Write `len` bytes from `buffer` to `handle`. Only standard output and
/// error can be written; anything else fails (`FALSE`). Overlapped writes
/// are not done: the last argument is ignored, and the write is complete
/// when this returns.
///
/// # Safety
/// `buffer` holds `len` bytes and `written` is null or writable, as the
/// Windows contract says. A program that lies faults in its own task.
unsafe extern "win64" fn write_file(
    handle: isize,
    buffer: *const u8,
    len: u32,
    written: *mut u32,
    _overlapped: *mut (),
) -> i32 {
    if file_of(handle).is_some() {
        // Files are opened for reading: the view is read-only today.
        return fail(ERROR_ACCESS_DENIED, 0);
    }
    if handle != STDOUT && handle != STDERR {
        return fail(ERROR_INVALID_HANDLE, 0);
    }
    if len > 0 {
        // SAFETY: the caller's promise, above.
        let bytes = unsafe { core::slice::from_raw_parts(buffer, len as usize) };
        for byte in bytes {
            PENDING.push(*byte);
        }
    }
    if !written.is_null() {
        // SAFETY: the caller's promise, above.
        unsafe { written.write_unaligned(len) };
    }
    1
}

/// End the program. A line still waiting for its end goes out first, and
/// the view, if it has one, is told it can end too.
extern "win64" fn exit_process(_code: u32) -> ! {
    if PENDING.waiting() {
        PENDING.flush(LINE);
    }
    let _ = send(VIEW, Message::new(view::DONE, [0; 4]));
    corrode_rt::exit()
}

/// One entry of the export table, laid out as `corrode_pe::exports` reads it.
#[repr(C)]
struct Export {
    dll: [u8; DLL_LEN],
    name: [u8; NAME_LEN],
    function: *const (),
}

#[repr(C)]
struct Table<const N: usize> {
    magic: [u8; 8],
    count: u32,
    size: u32,
    entries: [Export; N],
}

// SAFETY: read-only, and the pointers in it are only ever addresses.
unsafe impl<const N: usize> Sync for Table<N> {}

const _: () = assert!(core::mem::size_of::<Export>() == ENTRY);

/// A name in a fixed, zero-padded field.
const fn field<const N: usize>(text: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < text.len() {
        out[i] = text[i];
        i += 1;
    }
    out
}

const fn export(name: &[u8], function: *const ()) -> Export {
    Export {
        dll: field(b"kernel32.dll"),
        name: field(name),
        function,
    }
}

/// What this kernel32 offers, at `KERNEL32_BASE` (see `kernel32.ld`).
#[used]
#[link_section = ".verdigris.exports"]
static EXPORTS: Table<16> = Table {
    magic: MAGIC,
    count: 16,
    size: ENTRY as u32,
    entries: [
        export(b"GetStdHandle", get_std_handle as *const ()),
        export(b"WriteFile", write_file as *const ()),
        export(b"ExitProcess", exit_process as *const ()),
        export(b"CreateFileW", create_file_w as *const ()),
        export(b"ReadFile", read_file as *const ()),
        export(b"GetFileSize", get_file_size as *const ()),
        export(b"CloseHandle", close_handle as *const ()),
        export(b"GetLastError", get_last_error as *const ()),
        export(b"SetLastError", set_last_error as *const ()),
        export(b"FindFirstFileW", find_first_file_w as *const ()),
        export(b"FindNextFileW", find_next_file_w as *const ()),
        export(b"FindClose", find_close as *const ()),
        export(b"GetCurrentDirectoryW", get_current_directory_w as *const ()),
        export(b"SetCurrentDirectoryW", set_current_directory_w as *const ()),
        export(b"GetFullPathNameW", get_full_path_name_w as *const ()),
        export(b"GetCommandLineW", get_command_line_w as *const ()),
    ],
};
