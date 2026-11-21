//! Verdigris's kernel32: the smallest part of it a console program needs.
//!
//! `GetStdHandle`, `WriteFile` and `ExitProcess`, called by Windows code
//! with the Windows calling convention, in the Windows program's own task.
//! Standard output and error are the log endpoint, the program's only
//! capability (`corrode_abi::verdigris::LOG`): what a program writes comes
//! out as log lines under its name. Nothing here holds or asks for more.
//!
//! This is not a PE DLL yet (see `verdigris/DESIGN.md`, section 2): it is a
//! CorrodeOS program linked at `KERNEL32_BASE` by `kernel32.ld`, carrying
//! its export table at that address, which the kernel reads to bind a
//! program's imports. It never runs on its own; its entry point exists only
//! because a program has to have one.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use corrode_pe::exports::{DLL_LEN, ENTRY, MAGIC, NAME_LEN};
use corrode_rt::abi::log::{CAPACITY, LINE, TEXT};
use corrode_rt::abi::verdigris::LOG;
use corrode_rt::{send, Message};

corrode_rt::entry!(main);

/// Never called: the kernel starts a Windows program at its own entry.
fn main() {}

/// What `GetStdHandle` hands out: not capabilities, just numbers this
/// kernel32 recognises.
const STDIN: isize = 0x10;
const STDOUT: isize = 0x11;
const STDERR: isize = 0x12;
const INVALID_HANDLE_VALUE: isize = -1;

/// `STD_INPUT_HANDLE` and the rest, as the `u32` they arrive as.
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

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
    if handle != STDOUT && handle != STDERR {
        return 0;
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

/// End the program. A line still waiting for its end goes out first.
extern "win64" fn exit_process(_code: u32) -> ! {
    if PENDING.waiting() {
        PENDING.flush(LINE);
    }
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
static EXPORTS: Table<3> = Table {
    magic: MAGIC,
    count: 3,
    size: ENTRY as u32,
    entries: [
        export(b"GetStdHandle", get_std_handle as *const ()),
        export(b"WriteFile", write_file as *const ()),
        export(b"ExitProcess", exit_process as *const ()),
    ],
};
