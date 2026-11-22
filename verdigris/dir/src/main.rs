//! dir.exe: lists a folder, as `dir /b` would, with each file's size.
//!
//! The first Windows program here that a compiler wrote: Rust, for
//! `x86_64-pc-windows-msvc`, linked by `rust-lld` as `lld-link` into an
//! ordinary PE file, its imports from KERNEL32.dll by name (`raw-dylib`,
//! so no import library is needed). Verdigris loads it like any other.
//!
//! Its command line's first argument is the pattern, `*` if none:
//! `win /programs/dir/dir.exe` lists its own folder, `C:\`.

#![no_std]
#![no_main]

use core::ffi::c_void;
use core::ptr::null_mut;

type Handle = isize;

const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const INVALID_HANDLE_VALUE: Handle = -1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ERROR_NO_MORE_FILES: u32 = 18;

/// `WIN32_FIND_DATAW`, as Windows lays it out: its times are `FILETIME`s,
/// two `u32`s, so 4-aligned (a `u64` would move every field after it).
#[repr(C)]
struct FindData {
    attributes: u32,
    created: [u32; 2],
    accessed: [u32; 2],
    written: [u32; 2],
    size_high: u32,
    size_low: u32,
    reserved: [u32; 2],
    name: [u16; 260],
    short_name: [u16; 14],
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn WriteFile(file: Handle, bytes: *const u8, len: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    fn ExitProcess(code: u32) -> !;
    fn GetCommandLineW() -> *const u16;
    fn FindFirstFileW(pattern: *const u16, found: *mut FindData) -> Handle;
    fn FindNextFileW(search: Handle, found: *mut FindData) -> i32;
    fn FindClose(search: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// Standard output, a line at a time, built up in a buffer.
struct Out {
    line: [u8; 512],
    len: usize,
}

impl Out {
    fn text(&mut self, text: &[u8]) {
        for byte in text {
            if self.len < self.line.len() {
                self.line[self.len] = *byte;
                self.len += 1;
            }
        }
    }

    /// UTF-16 up to its zero, as bytes: what this system's names are.
    fn wide(&mut self, text: &[u16]) {
        for unit in text.iter().take_while(|unit| **unit != 0) {
            self.text(&[if *unit < 0x80 { *unit as u8 } else { b'?' }]);
        }
    }

    fn number(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.text(&digits[at..]);
    }

    fn end(&mut self) {
        self.text(b"\n");
        let mut written = 0;
        // SAFETY: a live buffer of `len` bytes; Verdigris's WriteFile reads
        // it and writes `written`.
        unsafe {
            WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), self.line.as_ptr(), self.len as u32, &mut written, null_mut());
        }
        self.len = 0;
    }
}

/// The first argument after the program's own name, or `*`.
fn pattern(line: *const u16, into: &mut [u16; 260]) {
    // Under a page, so no stack probe (`__chkstk`) is called for.
    let mut units = [0u16; 1024];
    let mut len = 0;
    // SAFETY: GetCommandLineW's string ends in a zero.
    while len < units.len() - 1 && unsafe { *line.add(len) } != 0 {
        units[len] = unsafe { *line.add(len) };
        len += 1;
    }
    let mut at = 0;
    // Past the program's name, quoted or not.
    if units.first() == Some(&(b'"' as u16)) {
        at = 1;
        while at < len && units[at] != b'"' as u16 {
            at += 1;
        }
        at += 1;
    } else {
        while at < len && units[at] != b' ' as u16 {
            at += 1;
        }
    }
    while at < len && units[at] == b' ' as u16 {
        at += 1;
    }
    let mut out = 0;
    while at < len && units[at] != b' ' as u16 && out < into.len() - 1 {
        into[out] = units[at];
        out += 1;
        at += 1;
    }
    if out == 0 {
        into[0] = b'*' as u16;
        out = 1;
    }
    into[out] = 0;
}

#[no_mangle]
extern "C" fn start() -> ! {
    let mut out = Out { line: [0; 512], len: 0 };
    let mut wanted = [0u16; 260];
    // SAFETY: no arguments; the string lives as long as the program.
    pattern(unsafe { GetCommandLineW() }, &mut wanted);

    // SAFETY: a zeroed FindData is a valid one to be written over.
    let mut found: FindData = unsafe { core::mem::zeroed() };
    // SAFETY: `wanted` ends in a zero; `found` is a writable FindData.
    let search = unsafe { FindFirstFileW(wanted.as_ptr(), &mut found) };
    if search == INVALID_HANDLE_VALUE {
        out.text(b"dir: ");
        out.wide(&wanted);
        out.text(b": error ");
        // SAFETY: no arguments.
        out.number(u64::from(unsafe { GetLastError() }));
        out.end();
        // SAFETY: ends the program.
        unsafe { ExitProcess(1) }
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    loop {
        out.wide(&found.name);
        if found.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            out.text(b"\\");
        } else {
            let size = (u64::from(found.size_high) << 32) | u64::from(found.size_low);
            out.text(b"  ");
            out.number(size);
            files += 1;
            bytes += size;
        }
        out.end();
        // SAFETY: a search FindFirstFileW opened; `found` as above.
        if unsafe { FindNextFileW(search, &mut found) } == 0 {
            break;
        }
    }
    // SAFETY: no arguments.
    let why = unsafe { GetLastError() };
    // SAFETY: the search is open.
    unsafe { FindClose(search) };
    out.number(files);
    out.text(b" files, ");
    out.number(bytes);
    out.text(b" bytes");
    if why != ERROR_NO_MORE_FILES {
        out.text(b"; the listing stopped early, error ");
        out.number(u64::from(why));
    }
    out.end();
    // SAFETY: ends the program.
    unsafe { ExitProcess(0) }
}

/// MSVC's mark that floating point is used, which `core` refers to. No C
/// runtime is linked to define it.
#[no_mangle]
#[used]
static _fltused: i32 = 0;

const _: () = assert!(core::mem::offset_of!(FindData, name) == 44 && size_of::<FindData>() == 592);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: ends the program.
    unsafe { ExitProcess(3) }
}
