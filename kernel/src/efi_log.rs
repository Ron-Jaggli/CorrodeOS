//! The boot log, kept in UEFI variables, for a real machine with no screen and
//! no serial cable.
//!
//! The firmware stores variables in its own flash and keeps the runtime code
//! that writes them alive after the OS takes over. So Corrode can hand its log
//! to the firmware, the machine can be powered off, and the next OS to boot --
//! Linux, say -- reads it back from `/sys/firmware/efi/efivars`. No storage
//! driver, USB stack or display involved.
//!
//! # What is kept
//!
//! Every byte printed is captured as it is printed: the first [`HEAD`] bytes in
//! full, since that is where bring-up reports, and after that a rolling
//! [`TAIL`] of the newest, since that is where a hang or a panic reports. At a
//! [`checkpoint`] the two are joined into one text and written as up to
//! [`CHUNKS`] variables of [`CHUNK`] bytes, `CorrodeLog0` upwards, under
//! [`VENDOR_TEXT`]. Chunks that have not changed since the last checkpoint
//! are not rewritten, and chunks a longer log left behind are deleted, so the
//! firmware's store holds one log, never an accumulation of them.
//!
//! The total is kept small on purpose. Firmware variable storage is a few
//! hundred KiB of flash shared with boot entries and Secure Boot keys, and
//! filling it is a known way to leave a board unable to boot. Nothing is
//! written unless the firmware says there is comfortably more room than this
//! needs.
//!
//! # Calling the firmware
//!
//! Limine never calls `SetVirtualAddressMap`, so the runtime services still
//! expect to run at their physical addresses. They get an address space of
//! their own for it: the kernel half, shared as every space shares it, plus an
//! identity map of exactly the regions the firmware marked as runtime. It is
//! only installed for the length of a call, on the calling CPU, so no task ever
//! sees those mappings. Everything handed to the firmware -- names, GUID, data,
//! out-parameters -- sits in frames identity-mapped the same way, so firmware
//! that assumes pointers are physical is right to. Calls run on a 64 KiB stack
//! of their own, because a task's 16 KiB is less than firmware is owed.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::x86_64;
use crate::limine;
use crate::mm::{frame, paging};

/// Bytes per variable. Well under every variable size limit seen in practice,
/// and it fits a data frame with room to spare.
const CHUNK: usize = 4000;
/// Variables used at most.
const CHUNKS: usize = 5;
/// Bytes kept from the start of the log.
const HEAD: usize = 12_000;
/// Room for the line that says how much of the middle was dropped.
const MARKER_ROOM: usize = 100;
/// Bytes kept from the end of the log.
const TAIL: usize = CHUNK * CHUNKS - HEAD - MARKER_ROOM;

/// `{8d3a1f0e-6c2b-4e57-9a41-c0bb5e7a2d19}`, chosen for Corrode's log alone.
const VENDOR: Guid = Guid {
    a: 0x8d3a_1f0e,
    b: 0x6c2b,
    c: 0x4e57,
    d: [0x9a, 0x41, 0xc0, 0xbb, 0x5e, 0x7a, 0x2d, 0x19],
};
pub const VENDOR_TEXT: &str = "8d3a1f0e-6c2b-4e57-9a41-c0bb5e7a2d19";

/// Survives power-off, and may be written after ExitBootServices.
const ATTRIBUTES: u32 = NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS;
const NON_VOLATILE: u32 = 1 << 0;
const BOOTSERVICE_ACCESS: u32 = 1 << 1;
const RUNTIME_ACCESS: u32 = 1 << 2;

/// Free variable storage to insist on beyond what the log itself takes.
const STORAGE_MARGIN: u64 = 32 * 1024;

/// Most pages of runtime regions to identity-map: 1 GiB. Far more than any
/// firmware needs, and a bound on a memory map that has gone wrong.
const MAX_RUNTIME_PAGES: u64 = 1 << 18;

/// Where the firmware stack goes in the firmware's address space: lower half,
/// above any physical address the identity map could need, and with an
/// unmapped page beneath it to catch an overflow.
const STACK_BASE: u64 = 0x0000_7f80_0000_0000;
const STACK_PAGES: u64 = 16;
const PAGE: u64 = 4096;

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

struct Capture {
    head: [u8; HEAD],
    head_len: usize,
    tail: [u8; TAIL],
    /// Bytes ever written to `tail`. Indexes the ring modulo `TAIL`.
    tail_total: usize,
}

static mut CAPTURE: Capture = Capture {
    head: [0; HEAD],
    head_len: 0,
    tail: [0; TAIL],
    tail_total: 0,
};

/// Keep `s` for the next checkpoint.
///
/// # Safety
/// Only from `serial::_print`, with the console lock held: that is what makes
/// this the only writer.
pub unsafe fn write(s: &str) {
    // SAFETY: the caller holds the console lock, and every other access to
    // `CAPTURE` is from `snapshot`, which takes it too.
    let capture = unsafe { &mut *core::ptr::addr_of_mut!(CAPTURE) };
    for &byte in s.as_bytes() {
        if capture.head_len < HEAD {
            capture.head[capture.head_len] = byte;
            capture.head_len += 1;
        } else {
            capture.tail[capture.tail_total % TAIL] = byte;
            capture.tail_total += 1;
        }
    }
}

/// Join head and tail into `out`, returning the length.
///
/// # Safety
/// The console lock must be held, so nothing writes to `CAPTURE` meanwhile.
unsafe fn snapshot(out: &mut [u8; CHUNK * CHUNKS]) -> usize {
    // SAFETY: as above.
    let capture = unsafe { &*core::ptr::addr_of!(CAPTURE) };
    let mut len = capture.head_len;
    out[..len].copy_from_slice(&capture.head[..len]);

    if capture.tail_total > TAIL {
        let mut marker = SliceWriter {
            buf: &mut out[len..len + MARKER_ROOM],
            len: 0,
        };
        let _ = write!(
            marker,
            "\n[efi-log: {} bytes from the middle not kept]\n",
            capture.tail_total - TAIL
        );
        len += marker.len;

        let start = capture.tail_total % TAIL;
        let older = &capture.tail[start..];
        let newer = &capture.tail[..start];
        out[len..len + older.len()].copy_from_slice(older);
        len += older.len();
        out[len..len + newer.len()].copy_from_slice(newer);
        len += newer.len();
    } else {
        let kept = &capture.tail[..capture.tail_total];
        out[len..len + kept.len()].copy_from_slice(kept);
        len += kept.len();
    }
    len
}

struct SliceWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let take = s.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

/// Held while a checkpoint is in progress. The firmware is not re-entrant, and
/// a second CPU arriving here mid-call -- two panics at once -- just skips.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Save the log so far into the firmware, and say how that went.
///
/// Safe to call from anywhere the machine is still running kernel code,
/// including the panic and trap paths. Costs a few flash writes, so it is
/// called at a handful of milestones, not per line.
pub fn checkpoint(stage: &str) {
    if BUSY.swap(true, Ordering::Acquire) {
        return;
    }
    // SAFETY: `BUSY` makes this the only CPU in here.
    let outcome = x86_64::without_interrupts(|| unsafe { save() });
    BUSY.store(false, Ordering::Release);

    match outcome {
        Ok(bytes) => {
            crate::println!("efi-log    : {stage}: {bytes} bytes kept as CorrodeLog*-{VENDOR_TEXT}")
        }
        Err(why) => crate::println!("efi-log    : {stage}: not kept -- {why}"),
    }
}

#[derive(Clone, Copy)]
enum Failure {
    NotUefi,
    NoMemmap,
    NoFrames,
    TooManyPages,
    BadSystemTable,
    BadRuntimeTable,
    OldFirmware(u32),
    VariableTooSmall(u64),
    StorageLow(u64),
    Firmware(&'static str, usize),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Failure::NotUefi => write!(f, "not booted through UEFI"),
            Failure::NoMemmap => write!(f, "the bootloader gave no UEFI memory map"),
            Failure::NoFrames => write!(f, "out of frames for the firmware's address space"),
            Failure::TooManyPages => write!(f, "runtime regions are implausibly large"),
            Failure::BadSystemTable => write!(f, "the system table signature is wrong"),
            Failure::BadRuntimeTable => write!(f, "the runtime services signature is wrong"),
            Failure::OldFirmware(revision) => write!(
                f,
                "UEFI {}.{} has no QueryVariableInfo",
                revision >> 16,
                revision & 0xffff
            ),
            Failure::VariableTooSmall(max) => {
                write!(f, "the firmware allows variables of only {max} bytes")
            }
            Failure::StorageLow(remaining) => write!(
                f,
                "only {remaining} bytes of variable storage free, not enough to be safe"
            ),
            Failure::Firmware(call, status) => {
                write!(f, "{call} failed, status {}", status & !ERROR_BIT)
            }
        }
    }
}

/// What a [`checkpoint`] needs, built at the first one.
struct Space {
    /// Root of the firmware's address space.
    root: u64,
    /// Top of the firmware stack, in that space.
    stack_top: u64,
    /// Physical address of the system table.
    system_table: u64,
    /// Frame holding the names, the GUID and the out-parameters.
    scratch: u64,
    /// One frame per chunk, holding what was last handed to the firmware.
    data: [u64; CHUNKS],
    /// What each variable holds now, as far as this boot knows.
    stored: [Stored; CHUNKS],
    /// Whether the storage check has passed.
    room_checked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stored {
    /// A previous boot may have left one.
    Unknown,
    Absent,
    /// This many bytes, which are the ones in the matching data frame.
    Bytes(usize),
}

static mut SPACE: Option<Space> = None;
/// Why building `SPACE` failed, so later checkpoints say so without retrying.
static mut BUILD_FAILED: Option<Failure> = None;
static mut SNAPSHOT: [u8; CHUNK * CHUNKS] = [0; CHUNK * CHUNKS];

/// # Safety
/// Only one CPU at a time, interrupts off: what `checkpoint` arranges.
unsafe fn save() -> Result<usize, Failure> {
    // SAFETY: `BUSY` serialises everything that touches these statics.
    let (space, build_failed, snapshot_buf) = unsafe {
        (
            &mut *core::ptr::addr_of_mut!(SPACE),
            &mut *core::ptr::addr_of_mut!(BUILD_FAILED),
            &mut *core::ptr::addr_of_mut!(SNAPSHOT),
        )
    };

    if let Some(failure) = *build_failed {
        return Err(failure);
    }
    if space.is_none() {
        match build() {
            Ok(built) => *space = Some(built),
            Err(failure) => {
                *build_failed = Some(failure);
                return Err(failure);
            }
        }
    }
    let Some(space) = space.as_mut() else {
        return Err(Failure::NoFrames);
    };

    // SAFETY: `with_console_lock` holds the console lock across the snapshot.
    let len = crate::serial::with_console_lock(|| unsafe { snapshot(snapshot_buf) });

    let mut session = Session {
        space: space as *mut Space,
        text: snapshot_buf.as_ptr(),
        len,
        result: Ok(len),
    };
    // SAFETY: the space maps the kernel half, so this code, `session` and the
    // snapshot stay reachable; the firmware's regions and the stack are what
    // it adds. Interrupts are off, so nothing runs on this CPU in between.
    unsafe {
        let previous = read_cr3();
        // UEFI promises the firmware SSE. Nothing of the kernel's lives in
        // those registers on this CPU for it to clobber.
        x86_64::enable_sse();
        paging::activate_root(space.root);
        call_on_stack(space.stack_top, session_main, &mut session);
        paging::activate_root(previous);
    }
    session.result
}

/// Everything one checkpoint does inside the firmware's address space.
struct Session {
    space: *mut Space,
    text: *const u8,
    len: usize,
    result: Result<usize, Failure>,
}

/// Runs on the firmware stack, in the firmware's address space.
extern "C" fn session_main(session: *mut Session) {
    // SAFETY: `save` passes a live `Session` and its `Space`, and nothing else
    // touches either while this runs.
    let session = unsafe { &mut *session };
    let space = unsafe { &mut *session.space };
    // SAFETY: the snapshot is a static of `CHUNK * CHUNKS` bytes, `len` of
    // them filled.
    let text = unsafe { core::slice::from_raw_parts(session.text, session.len) };
    session.result = unsafe { write_chunks(space, text) }.map(|()| session.len);
}

/// # Safety
/// Inside the firmware's address space, on its stack, interrupts off.
unsafe fn write_chunks(space: &mut Space, text: &[u8]) -> Result<(), Failure> {
    // SAFETY: the system table and the runtime services table are in runtime
    // regions, which are identity-mapped here; their signatures are checked
    // before any pointer they hold is followed.
    let runtime = unsafe {
        let system_table = space.system_table as *const u64;
        if system_table.read_volatile() != SYSTEM_TABLE_SIGNATURE {
            return Err(Failure::BadSystemTable);
        }
        let runtime = system_table.byte_add(88).read_volatile() as *const u64;
        if runtime.is_null() || runtime.read_volatile() != RUNTIME_SIGNATURE {
            return Err(Failure::BadRuntimeTable);
        }
        runtime
    };
    // SAFETY: offsets from the UEFI specification's EFI_RUNTIME_SERVICES.
    let (revision, set_variable, query_variable_info) = unsafe {
        (
            (runtime.byte_add(8) as *const u32).read_volatile(),
            core::mem::transmute::<u64, SetVariable>(runtime.byte_add(88).read_volatile()),
            runtime.byte_add(128).read_volatile(),
        )
    };

    let scratch = space.scratch;
    let guid = (scratch + GUID_OFFSET) as *const Guid;

    if !space.room_checked {
        if revision < 0x0002_0000 || query_variable_info == 0 {
            return Err(Failure::OldFirmware(revision));
        }
        // SAFETY: the table entry is non-null and the firmware is UEFI 2.0+,
        // which defines it. The out-parameters are in the scratch frame.
        let query: QueryVariableInfo = unsafe { core::mem::transmute(query_variable_info) };
        let out = (scratch + OUT_OFFSET) as *mut u64;
        let status = unsafe { query(ATTRIBUTES, out, out.add(1), out.add(2)) };
        if status & ERROR_BIT != 0 {
            return Err(Failure::Firmware("QueryVariableInfo", status));
        }
        // SAFETY: just written by the firmware.
        let (remaining, max_variable) = unsafe { (out.add(1).read(), out.add(2).read()) };
        // A variable's header and name count against its size too.
        if max_variable < CHUNK as u64 + 128 {
            return Err(Failure::VariableTooSmall(max_variable));
        }
        if remaining < (CHUNK * CHUNKS) as u64 + STORAGE_MARGIN {
            return Err(Failure::StorageLow(remaining));
        }
        space.room_checked = true;
    }

    let used = text.len().div_ceil(CHUNK).max(1);
    for chunk in 0..CHUNKS {
        let name = (scratch + chunk as u64 * NAME_STRIDE) as *const u16;
        if chunk < used {
            let start = chunk * CHUNK;
            let bytes = &text[start..text.len().min(start + CHUNK)];
            let frame = space.data[chunk] as *mut u8;
            // SAFETY: data frames are identity-mapped and a frame holds CHUNK.
            let held = unsafe { core::slice::from_raw_parts(frame, CHUNK) };
            if space.stored[chunk] == Stored::Bytes(bytes.len()) && held[..bytes.len()] == *bytes {
                continue;
            }
            // SAFETY: as above, and nothing else reads the frame meanwhile.
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), frame, bytes.len()) };
            // SAFETY: name, GUID and data are all identity-mapped.
            let status =
                unsafe { set_variable(name, guid, ATTRIBUTES, bytes.len(), frame.cast_const()) };
            if status & ERROR_BIT != 0 {
                space.stored[chunk] = Stored::Unknown;
                return Err(Failure::Firmware("SetVariable", status));
            }
            space.stored[chunk] = Stored::Bytes(bytes.len());
        } else if space.stored[chunk] != Stored::Absent {
            // Size zero deletes. Not finding one is the expected answer for a
            // chunk no boot ever reached, so only the attempt matters.
            // SAFETY: as above.
            let status = unsafe { set_variable(name, guid, ATTRIBUTES, 0, core::ptr::null()) };
            if status & ERROR_BIT != 0 && status & !ERROR_BIT != NOT_FOUND {
                return Err(Failure::Firmware("SetVariable (delete)", status));
            }
            space.stored[chunk] = Stored::Absent;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The firmware's address space
// ---------------------------------------------------------------------------

/// Where things sit in the scratch frame.
const NAME_STRIDE: u64 = 32;
const GUID_OFFSET: u64 = 1024;
const OUT_OFFSET: u64 = 1056;

fn build() -> Result<Space, Failure> {
    let system_table = limine::EFI_SYSTEM_TABLE
        .response
        .get()
        .ok_or(Failure::NotUefi)?;
    let memmap = limine::EFI_MEMMAP.response.get().ok_or(Failure::NoMemmap)?;
    let hhdm = limine::HHDM.response.get().ok_or(Failure::NoFrames)?.offset;

    let physical = |address: u64| address.checked_sub(hhdm).unwrap_or(address);
    let virtual_of = |address: u64| {
        if address >= hhdm {
            address
        } else {
            address + hhdm
        }
    };

    let mut mapper = paging::with_kernel_mapper(|kernel| kernel.fork_kernel_half())
        .flatten()
        .ok_or(Failure::NoFrames)?;

    let identity = |mapper: &mut paging::Mapper, pa: u64, flags: u64| {
        if mapper.map_4k(pa, pa, flags) {
            Ok(())
        } else {
            Err(Failure::NoFrames)
        }
    };

    // Every region the firmware will touch at runtime. Code is left writable:
    // firmware images keep their data inside the code region. Only this space
    // has these mappings, and only for the length of a call.
    let descriptors = virtual_of(memmap.memmap);
    let count = memmap.memmap_size / memmap.desc_size.max(1);
    let mut pages = 0u64;
    for i in 0..count {
        // SAFETY: Limine hands over `memmap_size` bytes of descriptors in
        // bootloader-reclaimable memory, which is never reclaimed and is in
        // the direct map. The stride is the firmware's own.
        let descriptor =
            unsafe { ((descriptors + i * memmap.desc_size) as *const Descriptor).read_unaligned() };
        if descriptor.attribute & MEMORY_RUNTIME == 0 {
            continue;
        }
        let flags = match descriptor.kind {
            RUNTIME_SERVICES_CODE | PAL_CODE => paging::WRITABLE,
            MEMORY_MAPPED_IO | MEMORY_MAPPED_IO_PORT_SPACE => {
                paging::WRITABLE | paging::NO_EXECUTE | paging::NO_CACHE | paging::WRITE_THROUGH
            }
            _ => paging::WRITABLE | paging::NO_EXECUTE,
        };
        for page in 0..descriptor.pages {
            pages += 1;
            if pages > MAX_RUNTIME_PAGES {
                return Err(Failure::TooManyPages);
            }
            identity(&mut mapper, descriptor.physical_start + page * PAGE, flags)?;
        }
    }

    // The system table itself, in case the firmware put it somewhere not
    // marked runtime. Two pages, since it may straddle a boundary.
    let system_table = physical(system_table.address);
    for page in 0..2 {
        identity(
            &mut mapper,
            (system_table & !(PAGE - 1)) + page * PAGE,
            paging::NO_EXECUTE,
        )?;
    }

    // Frames the firmware is handed pointers into.
    let fresh = |mapper: &mut paging::Mapper| -> Result<u64, Failure> {
        let pa = frame::alloc().ok_or(Failure::NoFrames)?.addr();
        identity(mapper, pa, paging::WRITABLE | paging::NO_EXECUTE)?;
        Ok(pa)
    };
    let scratch = fresh(&mut mapper)?;
    let mut data = [0u64; CHUNKS];
    for slot in data.iter_mut() {
        *slot = fresh(&mut mapper)?;
    }

    // SAFETY: a fresh frame from the allocator, reached through the direct map.
    unsafe {
        let base = (scratch + hhdm) as *mut u8;
        core::ptr::write_bytes(base, 0, PAGE as usize);
        for chunk in 0..CHUNKS {
            let name = base.add(chunk * NAME_STRIDE as usize) as *mut u16;
            for (i, unit) in "CorrodeLog".encode_utf16().enumerate() {
                name.add(i).write(unit);
            }
            name.add(10).write(b'0' as u16 + chunk as u16);
            // The terminating NUL is already there.
        }
        (base.add(GUID_OFFSET as usize) as *mut Guid).write(VENDOR);
    }

    // The stack: separate frames, contiguous here, with the page below
    // left unmapped.
    for page in 0..STACK_PAGES {
        let pa = frame::alloc().ok_or(Failure::NoFrames)?.addr();
        if !mapper.map_4k(
            STACK_BASE + (page + 1) * PAGE,
            pa,
            paging::WRITABLE | paging::NO_EXECUTE,
        ) {
            return Err(Failure::NoFrames);
        }
    }

    Ok(Space {
        root: mapper.root(),
        stack_top: STACK_BASE + (STACK_PAGES + 1) * PAGE,
        system_table,
        scratch,
        data,
        stored: [Stored::Unknown; CHUNKS],
        room_checked: false,
    })
}

// ---------------------------------------------------------------------------
// UEFI definitions
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    kind: u32,
    _pad: u32,
    physical_start: u64,
    virtual_start: u64,
    pages: u64,
    attribute: u64,
}

const RUNTIME_SERVICES_CODE: u32 = 5;
const MEMORY_MAPPED_IO: u32 = 11;
const MEMORY_MAPPED_IO_PORT_SPACE: u32 = 12;
const PAL_CODE: u32 = 13;
const MEMORY_RUNTIME: u64 = 1 << 63;

/// "IBI SYST" and "RUNTSERV", little-endian.
const SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
const RUNTIME_SIGNATURE: u64 = 0x5652_4553_544e_5552;

const ERROR_BIT: usize = 1 << 63;
const NOT_FOUND: usize = 14;

type SetVariable =
    unsafe extern "efiapi" fn(*const u16, *const Guid, u32, usize, *const u8) -> usize;
type QueryVariableInfo = unsafe extern "efiapi" fn(u32, *mut u64, *mut u64, *mut u64) -> usize;

// ---------------------------------------------------------------------------
// CPU state the firmware is owed
// ---------------------------------------------------------------------------

fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Call `f(argument)` with the stack pointer at `stack_top`, then come back to
/// this stack.
///
/// # Safety
/// `stack_top` must be 16-byte aligned and the top of mapped, writable,
/// otherwise unused memory in the current address space.
unsafe fn call_on_stack(stack_top: u64, f: extern "C" fn(*mut Session), argument: *mut Session) {
    // SAFETY: forwarded from the caller. r12 is callee-saved, so `f` hands it
    // back intact and it carries the old stack pointer across the call.
    unsafe {
        core::arch::asm!(
            "mov r12, rsp",
            "mov rsp, {top}",
            "call {f}",
            "mov rsp, r12",
            top = in(reg) stack_top,
            f = in(reg) f,
            in("rdi") argument,
            out("r12") _,
            clobber_abi("C"),
        );
    }
}
