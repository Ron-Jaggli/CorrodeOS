//! kernel32's questions about the program and the system: its module
//! handle and file name, the environment (empty), what kind of thing a
//! handle is, and the wall clock.
//!
//! The wall clock is the pair the panel gets (`window::CLOCK`): seconds
//! since 1970 at a monotonic nanosecond. The kernel writes it into the PEB
//! page before the program starts (DESIGN.md: "an IPC message at start,
//! not a new system call"; the PEB is how such start-up words reach a
//! program here). From then on the time is that pair plus the monotonic
//! clock. A machine with no real-time clock gives 0, and the time is then
//! 1970 plus the time since boot: wrong, but never going backwards.

use alloc::string::String;

// Where the kernel leaves the wall clock: seconds since 1970, and the
// monotonic nanosecond they were read at. Both 0 when there is no clock.
use corrode_rt::abi::verdigris::{PEB, WALL_AT, WALL_UNIX};

use crate::text::split;
use crate::{fail, file_of, get_command_line_w, set_last_error, One, STDERR, STDIN, STDOUT};


/// 100-nanosecond intervals from 1601 (a `FILETIME`'s epoch) to 1970.
const FILETIME_1970: u64 = 116_444_736_000_000_000;

const ERROR_MOD_NOT_FOUND: u32 = 126;
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_INVALID_HANDLE: u32 = 6;

const FILE_TYPE_UNKNOWN: u32 = 0;
const FILE_TYPE_DISK: u32 = 1;
const FILE_TYPE_CHAR: u32 = 2;

// ---------------------------------------------------------------- time

/// Nanoseconds since 1970, now.
fn wall_ns() -> u64 {
    // SAFETY: the kernel wrote both words before the PEB's page was mapped
    // (0 if it had no clock); the page is the program's own, read-only.
    let (unix, at) = unsafe { ((WALL_UNIX as *const u64).read(), (WALL_AT as *const u64).read()) };
    let now = corrode_rt::now_ns();
    unix.saturating_mul(1_000_000_000).saturating_add(now.saturating_sub(at))
}

/// Seconds since 1970, for `time`.
pub fn unix_seconds() -> i64 {
    (wall_ns() / 1_000_000_000) as i64
}

static STARTED: One<u64> = One::new(0);

/// The monotonic nanosecond `clock` counts from: the first time anything
/// asks, which is early in a C program's start-up.
pub fn started_ns() -> u64 {
    // SAFETY: one thread.
    let started = unsafe { STARTED.get() };
    if *started == 0 {
        *started = corrode_rt::now_ns();
    }
    *started
}

/// # Safety
/// `out` points at 8 writable bytes (a `FILETIME`).
pub unsafe extern "win64" fn get_system_time_as_file_time(out: *mut u64) {
    let filetime = FILETIME_1970 + wall_ns() / 100;
    // SAFETY: the caller's promise.
    unsafe { out.write_unaligned(filetime) };
}

// ---------------------------------------------------------------- the module

/// The program's image base, as the PEB says.
fn image_base() -> usize {
    // SAFETY: as in `wall_ns`: the kernel wrote it before the program ran.
    unsafe { ((PEB + 0x10) as *const u64).read() as usize }
}

/// `GetModuleHandleW`/`A`: null asks for the program itself. Other modules
/// (kernel32, msvcrt) are not PE images here, so naming one finds nothing.
pub extern "win64" fn get_module_handle(name: *const u8) -> usize {
    if name.is_null() {
        return image_base();
    }
    fail(ERROR_MOD_NOT_FOUND, 0)
}

/// The program's own name as Windows gives it: its full path, `C:\...`.
fn module_file_name() -> String {
    let line = get_command_line_w();
    let mut text = String::new();
    // SAFETY: kernel32's command line is zero-ended and lives as long as
    // the program.
    let units = (0..).map(|i| unsafe { line.add(i).read() }).take_while(|unit| *unit != 0);
    for character in char::decode_utf16(units) {
        text.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    let name = split(text.as_bytes()).into_iter().next().unwrap_or_default();
    let name = String::from_utf8_lossy(&name).into_owned();
    if name.len() > 2 && name.as_bytes()[1] == b':' {
        name
    } else {
        alloc::format!("C:\\{}", name.trim_start_matches('\\'))
    }
}

/// `GetModuleFileNameW`: the program's path, for the program's own module
/// (null or its base). Too small a buffer gets as much as fits, zero-ended,
/// and the error, as on Windows since XP.
///
/// # Safety
/// `buffer` has room for `size` UTF-16 units.
pub unsafe extern "win64" fn get_module_file_name_w(module: usize, buffer: *mut u16, size: u32) -> u32 {
    if module != 0 && module != image_base() {
        return fail(ERROR_MOD_NOT_FOUND, 0);
    }
    if size == 0 {
        return fail(ERROR_INSUFFICIENT_BUFFER, 0);
    }
    let units: alloc::vec::Vec<u16> = module_file_name().encode_utf16().collect();
    let room = size as usize - 1;
    let len = units.len().min(room);
    // SAFETY: the caller's promise: `size` units, and `len` < `size`.
    unsafe {
        core::ptr::copy_nonoverlapping(units.as_ptr(), buffer, len);
        buffer.add(len).write(0);
    }
    if units.len() > room {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return size;
    }
    set_last_error(0);
    len as u32
}

// ---------------------------------------------------------------- the rest

/// `GetEnvironmentVariableW`: a program here has no environment.
pub extern "win64" fn get_environment_variable_w(_name: *const u16, _buffer: *mut u16, _size: u32) -> u32 {
    fail(ERROR_ENVVAR_NOT_FOUND, 0)
}

/// `GetFileType`: the standard handles are character devices (the log, a
/// console), an open file is on a disk.
pub extern "win64" fn get_file_type(handle: isize) -> u32 {
    if matches!(handle, STDIN | STDOUT | STDERR) {
        set_last_error(0);
        FILE_TYPE_CHAR
    } else if file_of(handle).is_some() {
        set_last_error(0);
        FILE_TYPE_DISK
    } else {
        fail(ERROR_INVALID_HANDLE, FILE_TYPE_UNKNOWN)
    }
}
