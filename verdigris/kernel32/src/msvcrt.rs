//! Verdigris's msvcrt.dll: the C runtime a program built by mingw-w64 (or
//! an old MSVC) imports, as much of it as a console program needs to start,
//! print, allocate and end. Written here, in Rust, from the documented
//! behaviour; no C runtime's code is in it.
//!
//! The program's own start-up code (mingw's `crtexe.c`, linked into it)
//! does most of the work and calls in here: `__getmainargs` for `argc` and
//! `argv`, `_initterm` for its constructors, `_onexit` and `exit` for
//! `atexit`. Output goes where kernel32's `WriteFile` sends standard output
//! and error: the program's log.

use alloc::vec::Vec;
use core::ffi::c_void;

use crate::format::{format, Args};
use crate::process::{allocate, release, resize};
use crate::{exit_process, get_command_line_w, write_out, One};

// ---------------------------------------------------------------- data

/// msvcrt's exported variables. Programs reach each through its import
/// slot, which holds its address.
pub static mut MSVCRT_FMODE: i32 = 0;
pub static mut MSVCRT_COMMODE: i32 = 0;
/// The environment: an empty list. A Windows program here has none.
pub static mut MSVCRT_INITENV: *mut *mut u8 = core::ptr::null_mut();
/// The command line, in the ANSI code page (UTF-8).
pub static mut MSVCRT_ACMDLN: *mut u8 = core::ptr::null_mut();

static ERRNO: One<i32> = One::new(0);
static ARGV: One<(i32, *mut *mut u8)> = One::new((0, core::ptr::null_mut()));
static AT_EXIT: One<Vec<usize>> = One::new(Vec::new());

pub(crate) const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
pub(crate) const EBADF: i32 = 9;
pub(crate) const EOF: i32 = -1;

/// A string kept for the life of the program, zero-ended.
fn leak(bytes: &[u8]) -> *mut u8 {
    let block = allocate(bytes.len() + 1, true);
    if !block.is_null() {
        // SAFETY: a fresh block one longer than `bytes`, zeroed.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), block, bytes.len()) };
    }
    block
}

/// Set up what the program's start-up code reads before it calls in:
/// the command line, the arguments and the environment. Called by
/// kernel32's start routine, before the program's first instruction.
pub fn before_main() {
    // The command line, UTF-16 to UTF-8.
    let wide = get_command_line_w();
    let units = (0..).map(|i| {
        // SAFETY: kernel32's copy, which ends in a zero.
        unsafe { wide.add(i).read() }
    });
    let mut line = alloc::string::String::new();
    for character in char::decode_utf16(units.take_while(|unit| *unit != 0)) {
        line.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    let arguments: Vec<*mut u8> = split(line.as_bytes()).iter().map(|argument| leak(argument)).collect();
    let argv = allocate((arguments.len() + 1) * 8, true) as *mut *mut u8;
    let environment = allocate(8, true) as *mut *mut u8;
    if !argv.is_null() {
        for (i, argument) in arguments.iter().enumerate() {
            // SAFETY: room for every argument and a null after them.
            unsafe { argv.add(i).write(*argument) };
        }
    }
    // SAFETY: one thread, before the program runs.
    unsafe {
        *ARGV.get() = (arguments.len() as i32, argv);
        MSVCRT_INITENV = environment;
        MSVCRT_ACMDLN = leak(line.as_bytes());
    }
}

/// A command line split into arguments as the Microsoft C runtime splits
/// it: white space between arguments; quotes group and are removed; `2n`
/// backslashes before a quote are `n` backslashes and the quote counts,
/// `2n+1` are `n` and a literal quote; `""` inside quotes is a literal
/// quote; other backslashes are themselves. The first argument, the
/// program's name, takes quotes but not backslash rules.
pub fn split(line: &[u8]) -> Vec<Vec<u8>> {
    let mut arguments = Vec::new();
    let mut at = 0;
    // The program's name.
    let mut name = Vec::new();
    let mut quoted = false;
    while at < line.len() {
        let byte = line[at];
        if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && (byte == b' ' || byte == b'\t') {
            break;
        } else {
            name.push(byte);
        }
        at += 1;
    }
    arguments.push(name);
    loop {
        while at < line.len() && (line[at] == b' ' || line[at] == b'\t') {
            at += 1;
        }
        if at == line.len() {
            break;
        }
        let mut argument = Vec::new();
        let mut quoted = false;
        while at < line.len() {
            let byte = line[at];
            if !quoted && (byte == b' ' || byte == b'\t') {
                break;
            }
            if byte == b'\\' {
                let run = line[at..].iter().take_while(|byte| **byte == b'\\').count();
                at += run;
                if line.get(at) == Some(&b'"') {
                    argument.extend(core::iter::repeat_n(b'\\', run / 2));
                    if run % 2 == 1 {
                        argument.push(b'"');
                        at += 1;
                    }
                } else {
                    argument.extend(core::iter::repeat_n(b'\\', run));
                }
                continue;
            }
            if byte == b'"' {
                if quoted && line.get(at + 1) == Some(&b'"') {
                    argument.push(b'"');
                    at += 2;
                    continue;
                }
                quoted = !quoted;
                at += 1;
                continue;
            }
            argument.push(byte);
            at += 1;
        }
        arguments.push(argument);
    }
    arguments
}

/// `__getmainargs`: `argc`, `argv` and the environment for `main`. The
/// wildcard flag asks for `*` and `?` in arguments to be expanded against
/// the files; that is not done here, so they reach `main` as typed.
///
/// # Safety
/// The three pointers are writable, as the contract says.
pub unsafe extern "win64" fn getmainargs(
    argc: *mut i32,
    argv: *mut *mut *mut u8,
    env: *mut *mut *mut u8,
    _wildcard: i32,
    _startup: *mut c_void,
) -> i32 {
    // SAFETY: one thread; `before_main` set them.
    let (count, list) = unsafe { *ARGV.get() };
    if list.is_null() {
        return -1;
    }
    // SAFETY: the caller's promise.
    unsafe {
        argc.write(count);
        argv.write(list);
        env.write(MSVCRT_INITENV);
    }
    0
}

/// Call each function in `[begin, end)`, skipping nulls: C++ constructors
/// and C initialisers, as the linker gathered them.
///
/// # Safety
/// The range holds function addresses or nulls, as the linker writes it.
pub unsafe extern "win64" fn initterm(begin: *const usize, end: *const usize) {
    let mut at = begin;
    while at < end {
        // SAFETY: the caller's promise.
        let function = unsafe { at.read() };
        if function != 0 {
            // SAFETY: an initialiser the program's linker listed.
            let function: extern "win64" fn() = unsafe { core::mem::transmute(function) };
            function();
        }
        // SAFETY: still inside, or at the end of, the range.
        at = unsafe { at.add(1) };
    }
}

/// Register a function to run at `exit`, last registered first.
pub extern "win64" fn onexit(function: usize) -> usize {
    // SAFETY: one thread; nothing else holds it.
    unsafe { AT_EXIT.get().push(function) };
    function
}

pub extern "win64" fn atexit(function: usize) -> i32 {
    if onexit(function) == 0 { -1 } else { 0 }
}

/// Run the `atexit` functions, each once.
fn run_at_exit() {
    // SAFETY: one thread; each is taken out before it is called, so one
    // that registers another, or calls exit, finds the list consistent.
    while let Some(function) = unsafe { AT_EXIT.get().pop() } {
        // SAFETY: a function the program registered.
        let function: extern "win64" fn() = unsafe { core::mem::transmute(function) };
        function();
    }
}

pub extern "win64" fn exit(code: i32) -> ! {
    run_at_exit();
    exit_process(code as u32)
}

/// `_exit`: end without running `atexit` functions.
pub extern "win64" fn exit_now(code: i32) -> ! {
    exit_process(code as u32)
}

/// `_cexit`: what `exit` does, short of ending.
pub extern "win64" fn cexit() {
    run_at_exit();
}

/// `_amsg_exit`: a runtime error, numbered as Microsoft's are.
pub extern "win64" fn amsg_exit(number: i32) -> ! {
    let mut line = alloc::string::String::new();
    let _ = core::fmt::write(&mut line, format_args!("runtime error R60{number:02}\n"));
    write_out(2, line.as_bytes());
    exit_process(255)
}

/// As Microsoft's runtime ends a program that aborts: exit code 3.
pub extern "win64" fn abort() -> ! {
    write_out(2, b"abnormal program termination\n");
    exit_process(3)
}

pub extern "win64" fn errno() -> *mut i32 {
    ERRNO.as_ptr()
}

pub(crate) fn set_errno(value: i32) {
    // SAFETY: one thread.
    unsafe { *ERRNO.get() = value };
}

/// `_lock`/`_unlock`: the runtime's own locks. One thread, so holding one
/// is never contended and there is nothing to record.
pub extern "win64" fn lock(_which: i32) {}

pub extern "win64" fn set_app_type(_kind: i32) {}

pub extern "win64" fn setusermatherr(_handler: usize) {}

/// Never called: there are no exceptions to unwind here (DESIGN.md
/// section 2). If it were, "keep searching" is the answer that claims
/// nothing.
pub extern "win64" fn c_specific_handler(_record: usize, _frame: usize, _context: usize, _dispatch: usize) -> i32 {
    1 // ExceptionContinueSearch
}

/// Signal handlers are kept and handed back. Nothing raises a signal here
/// but the program itself (and `raise` is not offered yet).
static SIGNALS: One<[usize; 23]> = One::new([0; 23]);

pub extern "win64" fn signal(number: i32, handler: usize) -> usize {
    const SIG_ERR: usize = usize::MAX;
    match number {
        2 | 4 | 8 | 11 | 15 | 21 | 22 => {
            // SAFETY: one thread.
            core::mem::replace(unsafe { &mut SIGNALS.get()[number as usize] }, handler)
        }
        _ => {
            set_errno(EINVAL);
            SIG_ERR
        }
    }
}

// ---------------------------------------------------------------- locale

pub extern "win64" fn lc_codepage() -> u32 {
    crate::process::CP_UTF8
}

/// The longest character in bytes: four, in UTF-8.
pub extern "win64" fn mb_cur_max() -> i32 {
    4
}

/// `struct lconv` of the "C" locale.
#[repr(C)]
pub struct Lconv {
    strings: [*const u8; 10],
    numbers: [u8; 8],
    wide: [*const u16; 6],
}

// SAFETY: read-only.
unsafe impl Sync for Lconv {}

static POINT: &[u8] = b".\0";
static EMPTY: &[u8] = b"\0";
static WIDE_POINT: [u16; 2] = [b'.' as u16, 0];
static WIDE_EMPTY: [u16; 1] = [0];

static C_LOCALE: Lconv = Lconv {
    strings: [
        POINT.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
        EMPTY.as_ptr(),
    ],
    numbers: [127; 8],
    wide: [
        WIDE_POINT.as_ptr(),
        WIDE_EMPTY.as_ptr(),
        WIDE_EMPTY.as_ptr(),
        WIDE_EMPTY.as_ptr(),
        WIDE_EMPTY.as_ptr(),
        WIDE_EMPTY.as_ptr(),
    ],
};

pub extern "win64" fn localeconv() -> *const Lconv {
    &C_LOCALE
}

/// Microsoft's texts for its error numbers.
pub extern "win64" fn strerror(number: i32) -> *const u8 {
    let text: &'static [u8] = match number {
        0 => b"No error\0",
        1 => b"Operation not permitted\0",
        2 => b"No such file or directory\0",
        3 => b"No such process\0",
        4 => b"Interrupted function call\0",
        5 => b"Input/output error\0",
        7 => b"Arg list too long\0",
        8 => b"Exec format error\0",
        9 => b"Bad file descriptor\0",
        12 => b"Not enough space\0",
        13 => b"Permission denied\0",
        17 => b"File exists\0",
        20 => b"Not a directory\0",
        21 => b"Is a directory\0",
        22 => b"Invalid argument\0",
        24 => b"Too many open files\0",
        28 => b"No space left on device\0",
        32 => b"Broken pipe\0",
        33 => b"Domain error\0",
        34 => b"Result too large\0",
        _ => b"Unknown error\0",
    };
    text.as_ptr()
}

// ---------------------------------------------------------------- streams

/// msvcrt's `FILE` (48 bytes). Only `_file` means anything here: which
/// standard stream it is, or a file's handle (`stdio`).
#[repr(C)]
pub struct File {
    ptr: *mut u8,
    count: i32,
    base: *mut u8,
    flag: i32,
    file: i32,
    charbuf: i32,
    bufsiz: i32,
    tmpfname: *mut u8,
}

pub(crate) const fn stream(file: i32) -> File {
    File {
        ptr: core::ptr::null_mut(),
        count: 0,
        base: core::ptr::null_mut(),
        flag: 0,
        file,
        charbuf: 0,
        bufsiz: 0,
        tmpfname: core::ptr::null_mut(),
    }
}

/// stdin, stdout, stderr, as `__iob_func` hands them out.
static IOB: One<[File; 3]> = One::new([stream(0), stream(1), stream(2)]);

pub extern "win64" fn iob_func() -> *mut File {
    IOB.as_ptr().cast()
}

/// Whether `file` is stdin, stdout or stderr.
pub(crate) fn is_standard(file: *const File) -> bool {
    let first = IOB.as_ptr() as *const File;
    // SAFETY: only compared, never read through.
    (0..3).any(|i| core::ptr::eq(unsafe { first.add(i) }, file))
}

/// Which standard stream `file` is, if it is one that can be written.
fn writable(file: *const File) -> Option<i32> {
    let first = IOB.as_ptr() as *const File;
    // SAFETY: only compared, never read through.
    let (out, error) = unsafe { (first.add(1), first.add(2)) };
    if file == out {
        Some(1)
    } else if file == error {
        Some(2)
    } else {
        None
    }
}

/// # Safety
/// `file` is a stream this runtime gave out, or the program's own garbage,
/// which is refused, never read.
pub unsafe extern "win64" fn fputc(character: i32, file: *mut File) -> i32 {
    match writable(file) {
        Some(which) => {
            write_out(which, &[character as u8]);
            character & 0xff
        }
        None => {
            set_errno(EBADF);
            EOF
        }
    }
}

pub extern "win64" fn putchar(character: i32) -> i32 {
    write_out(1, &[character as u8]);
    character & 0xff
}

/// # Safety
/// `data` holds `size * count` bytes.
pub unsafe extern "win64" fn fwrite(data: *const u8, size: usize, count: usize, file: *mut File) -> usize {
    let Some(which) = writable(file) else {
        set_errno(EBADF);
        return 0;
    };
    let Some(len) = size.checked_mul(count) else {
        set_errno(EINVAL);
        return 0;
    };
    if len > 0 {
        // SAFETY: the caller's promise.
        write_out(which, unsafe { core::slice::from_raw_parts(data, len) });
    }
    count
}

/// # Safety
/// `text` is a zero-ended string.
pub unsafe extern "win64" fn fputs(text: *const u8, file: *mut File) -> i32 {
    let Some(which) = writable(file) else {
        set_errno(EBADF);
        return EOF;
    };
    // SAFETY: the caller's promise.
    write_out(which, unsafe { core::ffi::CStr::from_ptr(text.cast()) }.to_bytes());
    0
}

/// # Safety
/// `text` is a zero-ended string.
pub unsafe extern "win64" fn puts(text: *const u8) -> i32 {
    // SAFETY: the caller's promise.
    write_out(1, unsafe { core::ffi::CStr::from_ptr(text.cast()) }.to_bytes());
    write_out(1, b"\n");
    0
}

/// Every line written so far has gone already; a part line waits for its
/// end (or for exit), as the log takes whole lines.
pub extern "win64" fn fflush(_file: *mut File) -> i32 {
    0
}

/// The formatting entry points, after the varargs shims below.
///
/// # Safety
/// As [`format`].
pub unsafe extern "win64" fn vfprintf(file: *mut File, text: *const u8, args: *const u64) -> i32 {
    let Some(which) = writable(file) else {
        set_errno(EBADF);
        return -1;
    };
    let mut line = Vec::new();
    // SAFETY: the caller's promise.
    let count = unsafe { format(text, &mut Args(args), &mut |byte| line.push(byte)) };
    write_out(which, &line);
    count as i32
}

/// # Safety
/// As [`format`].
pub unsafe extern "win64" fn vprintf(text: *const u8, args: *const u64) -> i32 {
    // SAFETY: forwarded.
    unsafe { vfprintf(iob_func().add(1), text, args) }
}

/// `vsnprintf`-style into a buffer of `room` bytes: at most `room - 1`
/// characters and a zero. Returns how many it would have written, or -1 if
/// they did not fit (Microsoft's `_vsnprintf` rule).
///
/// # Safety
/// As [`format`]; `buffer` holds `room` bytes.
pub unsafe extern "win64" fn vsnprintf(buffer: *mut u8, room: usize, text: *const u8, args: *const u64) -> i32 {
    let mut written = 0usize;
    // SAFETY: the caller's promise.
    let count = unsafe {
        format(text, &mut Args(args), &mut |byte| {
            if written < room {
                buffer.add(written).write(byte);
                written += 1;
            }
        })
    };
    if count < room {
        // SAFETY: inside the buffer.
        unsafe { buffer.add(count).write(0) };
        count as i32
    } else {
        -1
    }
}

/// # Safety
/// As [`format`]; `buffer` is big enough, as `sprintf`'s contract says.
pub unsafe extern "win64" fn vsprintf(buffer: *mut u8, text: *const u8, args: *const u64) -> i32 {
    // SAFETY: the caller's promise.
    unsafe { vsnprintf(buffer, usize::MAX, text, args) }
}

// The variadic entries. A variadic Windows function receives its first
// four arguments in rcx, rdx, r8 and r9 and the rest on the stack, with 32
// bytes reserved above the return address for the four. Storing the four
// there makes every argument one 8-byte slot in a row, which is what a
// `va_list` points at; then the `v` form is called with its address.

/// `printf(format, ...)`
#[unsafe(naked)]
pub unsafe extern "win64" fn printf() {
    core::arch::naked_asm!(
        "mov [rsp + 8], rcx",
        "mov [rsp + 16], rdx",
        "mov [rsp + 24], r8",
        "mov [rsp + 32], r9",
        "sub rsp, 40",
        "lea rdx, [rsp + 40 + 16]",
        "call {v}",
        "add rsp, 40",
        "ret",
        v = sym vprintf,
    )
}

/// `fprintf(file, format, ...)`
#[unsafe(naked)]
pub unsafe extern "win64" fn fprintf() {
    core::arch::naked_asm!(
        "mov [rsp + 8], rcx",
        "mov [rsp + 16], rdx",
        "mov [rsp + 24], r8",
        "mov [rsp + 32], r9",
        "sub rsp, 40",
        "lea r8, [rsp + 40 + 24]",
        "call {v}",
        "add rsp, 40",
        "ret",
        v = sym vfprintf,
    )
}

/// `sprintf(buffer, format, ...)`
#[unsafe(naked)]
pub unsafe extern "win64" fn sprintf() {
    core::arch::naked_asm!(
        "mov [rsp + 8], rcx",
        "mov [rsp + 16], rdx",
        "mov [rsp + 24], r8",
        "mov [rsp + 32], r9",
        "sub rsp, 40",
        "lea r8, [rsp + 40 + 24]",
        "call {v}",
        "add rsp, 40",
        "ret",
        v = sym vsprintf,
    )
}

/// `_snprintf(buffer, room, format, ...)`
#[unsafe(naked)]
pub unsafe extern "win64" fn snprintf() {
    core::arch::naked_asm!(
        "mov [rsp + 8], rcx",
        "mov [rsp + 16], rdx",
        "mov [rsp + 24], r8",
        "mov [rsp + 32], r9",
        "sub rsp, 40",
        "lea r9, [rsp + 40 + 32]",
        "call {v}",
        "add rsp, 40",
        "ret",
        v = sym vsnprintf,
    )
}

// ---------------------------------------------------------------- memory

pub extern "win64" fn malloc(size: usize) -> *mut u8 {
    let block = allocate(size.max(1), false);
    if block.is_null() {
        set_errno(ENOMEM);
    }
    block
}

pub extern "win64" fn calloc(count: usize, size: usize) -> *mut u8 {
    let Some(len) = count.checked_mul(size) else {
        set_errno(ENOMEM);
        return core::ptr::null_mut();
    };
    let block = allocate(len.max(1), true);
    if block.is_null() {
        set_errno(ENOMEM);
    }
    block
}

/// # Safety
/// `block` is null or from this runtime's allocator, not yet freed.
pub unsafe extern "win64" fn realloc(block: *mut u8, size: usize) -> *mut u8 {
    if size == 0 && !block.is_null() {
        // SAFETY: the caller's promise.
        unsafe { release(block) };
        return core::ptr::null_mut();
    }
    // SAFETY: the caller's promise.
    let new = unsafe { resize(block, size.max(1), false) };
    if new.is_null() {
        set_errno(ENOMEM);
    }
    new
}

/// # Safety
/// As [`realloc`].
pub unsafe extern "win64" fn free(block: *mut u8) {
    // SAFETY: the caller's promise.
    unsafe { release(block) };
}

// ---------------------------------------------------------------- strings

/// # Safety
/// The usual: `len` bytes at each.
pub unsafe extern "win64" fn memcpy(to: *mut u8, from: *const u8, len: usize) -> *mut u8 {
    // SAFETY: the caller's promise. `copy`, not `copy_nonoverlapping`: a
    // program that overlaps them gets memmove's answer, not garbage.
    unsafe { core::ptr::copy(from, to, len) };
    to
}

/// # Safety
/// As [`memcpy`].
pub unsafe extern "win64" fn memmove(to: *mut u8, from: *const u8, len: usize) -> *mut u8 {
    // SAFETY: the caller's promise.
    unsafe { core::ptr::copy(from, to, len) };
    to
}

/// # Safety
/// `len` bytes at `to`.
pub unsafe extern "win64" fn memset(to: *mut u8, value: i32, len: usize) -> *mut u8 {
    // SAFETY: the caller's promise.
    unsafe { core::ptr::write_bytes(to, value as u8, len) };
    to
}

/// # Safety
/// `len` bytes at each.
pub unsafe extern "win64" fn memcmp(a: *const u8, b: *const u8, len: usize) -> i32 {
    for i in 0..len {
        // SAFETY: the caller's promise.
        let (x, y) = unsafe { (a.add(i).read(), b.add(i).read()) };
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

/// # Safety
/// A zero-ended string.
pub unsafe extern "win64" fn strlen(text: *const u8) -> usize {
    // SAFETY: the caller's promise.
    unsafe { core::ffi::CStr::from_ptr(text.cast()) }.count_bytes()
}

/// # Safety
/// A zero-ended wide string.
pub unsafe extern "win64" fn wcslen(text: *const u16) -> usize {
    // SAFETY: the caller's promise; read up to the zero.
    (0..).find(|i| unsafe { text.add(*i).read_unaligned() } == 0).unwrap_or(0)
}

/// # Safety
/// Zero-ended strings, or at least `most` bytes each.
pub unsafe extern "win64" fn strncmp(a: *const u8, b: *const u8, most: usize) -> i32 {
    for i in 0..most {
        // SAFETY: the caller's promise; stops at the first zero.
        let (x, y) = unsafe { (a.add(i).read(), b.add(i).read()) };
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    0
}

/// # Safety
/// Zero-ended strings.
pub unsafe extern "win64" fn strcmp(a: *const u8, b: *const u8) -> i32 {
    // SAFETY: the caller's promise.
    unsafe { strncmp(a, b, usize::MAX) }
}

/// # Safety
/// A zero-ended string; `to` big enough.
pub unsafe extern "win64" fn strcpy(to: *mut u8, from: *const u8) -> *mut u8 {
    // SAFETY: the caller's promise.
    unsafe {
        let len = strlen(from);
        core::ptr::copy(from, to, len + 1);
    }
    to
}
