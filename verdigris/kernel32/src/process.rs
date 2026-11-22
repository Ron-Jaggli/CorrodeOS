//! kernel32 beyond files and output: how a program starts, the heap, thread
//! storage, critical sections, code pages, time.
//!
//! A Windows program here has one thread (CorrodeOS has no ring 3 threads
//! yet), so a critical section is only ever entered by its owner and thread
//! storage has one set of slots. Each is written to be right for one
//! thread, not to pretend to be right for more.

use core::alloc::Layout;
use core::ffi::c_void;

use corrode_rt::abi::verdigris::{HEAP, HEAP_PIECES, PEB, POOL, POOL_PAGES, START_ENTRY, START_TLS_CALLBACKS};

use crate::{exit_process, fail, set_last_error, One};

pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;

// ---------------------------------------------------------------- start

/// Where a Windows program begins: the kernel starts it here, as Windows
/// starts every program in its own loader first. The program's TLS
/// callbacks run (`DLL_PROCESS_ATTACH`), then its entry point; what that
/// returns is its exit code, as on Windows, where `mainCRTStartup` may
/// return to the thread that called it.
pub extern "win64" fn start() -> ! {
    type Callback = extern "win64" fn(*mut c_void, u32, *mut c_void);
    type Entry = extern "win64" fn() -> u32;
    const DLL_PROCESS_ATTACH: u32 = 1;

    // The heap grows from the program's pool whenever an allocation, the C
    // runtime's or this file's own, finds no room. A program given no pool
    // (the boot checks' console.exe) has no heap, and no C runtime set up.
    corrode_rt::heap::on_empty(grow);
    if grow(0) {
        crate::msvcrt::before_main();
    }
    // SAFETY: the kernel wrote both words, and the PEB's image base, before
    // the PEB's page was mapped; they are the program's own addresses.
    let (entry, callbacks, base) = unsafe {
        (
            (START_ENTRY as *const u64).read(),
            (START_TLS_CALLBACKS as *const u64).read(),
            ((PEB + 0x10) as *const u64).read(),
        )
    };
    if callbacks != 0 {
        let mut at = callbacks as *const u64;
        loop {
            // SAFETY: the loader checked the array starts in the image; it
            // ends at a zero, as the format says. A program that lies
            // faults in its own task.
            let callback = unsafe { at.read() };
            if callback == 0 {
                break;
            }
            // SAFETY: an address the program says is a TLS callback; it is
            // the program's own code to run, as Windows runs it.
            let callback: Callback = unsafe { core::mem::transmute(callback as usize) };
            callback(base as *mut c_void, DLL_PROCESS_ATTACH, core::ptr::null_mut());
            // SAFETY: as above.
            at = unsafe { at.add(1) };
        }
    }
    // SAFETY: the entry point the loader checked lies in executable code.
    let entry: Entry = unsafe { core::mem::transmute(entry as usize) };
    let code = entry();
    exit_process(code)
}

// ---------------------------------------------------------------- the heap

/// Pieces of memory bought from the pool so far.
static PIECES: One<usize> = One::new(0);
/// Pages in the first piece; each later one is at least as big as the last.
const FIRST_PIECE: usize = 64;
static LAST_PIECE: One<usize> = One::new(0);
/// Pages the pieces so far took from the pool.
static BOUGHT: One<usize> = One::new(0);

/// Every block carries its size and alignment in the 16 bytes before it,
/// so `free`, which is told only the address, can give it back.
const HEADER: usize = 16;

/// Buy more memory for the heap from the pool, enough for `bytes`.
pub fn grow(bytes: usize) -> bool {
    // SAFETY: one thread, and no reference outlives this function.
    let (pieces, last, bought) = unsafe { (PIECES.get(), LAST_PIECE.get(), BOUGHT.get()) };
    if *pieces == HEAP_PIECES {
        return false;
    }
    let needed = (bytes.saturating_add(HEADER)).div_ceil(4096) + 1;
    let left = (POOL_PAGES as usize).saturating_sub(*bought);
    if needed > left {
        return false;
    }
    // Each piece twice the last, so a few pieces go far; but never more
    // than the budget has left, so the last of it is not wasted.
    let wanted = needed.max(FIRST_PIECE).max(*last * 2).min(left);
    // If the pool takes something of its own for a piece, exactly what is
    // needed may still fit.
    for pages in [wanted, needed] {
        if corrode_rt::heap::init(POOL, HEAP + *pieces, pages).is_ok() {
            *pieces += 1;
            *last = pages;
            *bought += pages;
            return true;
        }
    }
    false
}

/// `size` bytes, aligned to 16 (as Windows' heap and malloc give), or null.
pub fn allocate(size: usize, zeroed: bool) -> *mut u8 {
    let Some(total) = size.checked_add(HEADER) else {
        return core::ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total, 16) else {
        return core::ptr::null_mut();
    };
    // SAFETY: a layout of non-zero size. The heap grows itself if it must
    // (`on_empty`, set in `start`).
    let block = unsafe {
        if zeroed {
            alloc::alloc::alloc_zeroed(layout)
        } else {
            alloc::alloc::alloc(layout)
        }
    };
    if block.is_null() {
        return block;
    }
    // SAFETY: the block is `total` bytes, at least 16.
    unsafe {
        (block as *mut usize).write(total);
        block.add(HEADER)
    }
}

/// The size `allocate` was asked for, of a block it gave.
///
/// # Safety
/// `block` came from [`allocate`] and has not been released.
pub unsafe fn size_of(block: *mut u8) -> usize {
    // SAFETY: the caller's promise; the header is just before the block.
    unsafe { (block.sub(HEADER) as *const usize).read() - HEADER }
}

/// Give back a block `allocate` gave. Null is nothing to do.
///
/// # Safety
/// `block` is null or came from [`allocate`] and has not been released.
pub unsafe fn release(block: *mut u8) {
    if block.is_null() {
        return;
    }
    // SAFETY: the caller's promise.
    unsafe {
        let start = block.sub(HEADER);
        let total = (start as *const usize).read();
        alloc::alloc::dealloc(start, Layout::from_size_align_unchecked(total, 16));
    }
}

/// Move a block to one of `size` bytes, keeping what fits. Null `block` is
/// `allocate`; on failure the old block stays as it was.
///
/// # Safety
/// As [`release`].
pub unsafe fn resize(block: *mut u8, size: usize, zeroed: bool) -> *mut u8 {
    if block.is_null() {
        return allocate(size, zeroed);
    }
    let new = allocate(size, zeroed);
    if !new.is_null() {
        // SAFETY: both blocks are ours and at least the copied length.
        unsafe {
            let old = size_of(block);
            core::ptr::copy_nonoverlapping(block, new, old.min(size));
            release(block);
        }
    }
    new
}

/// The one heap `GetProcessHeap` names.
const PROCESS_HEAP: isize = 0x7000;
const HEAP_ZERO_MEMORY: u32 = 0x8;
const HEAP_REALLOC_IN_PLACE_ONLY: u32 = 0x10;

pub extern "win64" fn get_process_heap() -> isize {
    PROCESS_HEAP
}

pub extern "win64" fn heap_alloc(heap: isize, flags: u32, size: usize) -> *mut u8 {
    if heap != PROCESS_HEAP {
        return fail(ERROR_INVALID_PARAMETER, core::ptr::null_mut());
    }
    let block = allocate(size, flags & HEAP_ZERO_MEMORY != 0);
    if block.is_null() {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
    }
    block
}

/// # Safety
/// `block` is null or one of this heap's, as the Windows contract says.
pub unsafe extern "win64" fn heap_free(heap: isize, _flags: u32, block: *mut u8) -> i32 {
    if heap != PROCESS_HEAP {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    // SAFETY: the caller's promise.
    unsafe { release(block) };
    1
}

/// # Safety
/// As [`heap_free`].
pub unsafe extern "win64" fn heap_re_alloc(heap: isize, flags: u32, block: *mut u8, size: usize) -> *mut u8 {
    if heap != PROCESS_HEAP || block.is_null() {
        return fail(ERROR_INVALID_PARAMETER, core::ptr::null_mut());
    }
    if flags & HEAP_REALLOC_IN_PLACE_ONLY != 0 {
        // The block may not move: it stays where it is if it is already
        // big enough, and the call fails, the block untouched, if not.
        // SAFETY: the caller's promise.
        let old = unsafe { size_of(block) };
        if size <= old {
            return block;
        }
        return fail(ERROR_NOT_ENOUGH_MEMORY, core::ptr::null_mut());
    }
    // SAFETY: the caller's promise.
    let new = unsafe { resize(block, size, flags & HEAP_ZERO_MEMORY != 0) };
    if new.is_null() {
        set_last_error(ERROR_NOT_ENOUGH_MEMORY);
    }
    new
}

// ---------------------------------------------------------------- thread storage

const TLS_SLOTS: usize = 64;
const TLS_OUT_OF_INDEXES: u32 = 0xffff_ffff;
static TLS_USED: One<u64> = One::new(0);
static TLS_VALUES: One<[usize; TLS_SLOTS]> = One::new([0; TLS_SLOTS]);

pub extern "win64" fn tls_alloc() -> u32 {
    // SAFETY: one thread; nothing else holds it.
    let used = unsafe { TLS_USED.get() };
    match (0..TLS_SLOTS).find(|slot| *used & (1 << slot) == 0) {
        Some(slot) => {
            *used |= 1 << slot;
            // SAFETY: as above.
            unsafe { TLS_VALUES.get()[slot] = 0 };
            slot as u32
        }
        None => fail(ERROR_NOT_ENOUGH_MEMORY, TLS_OUT_OF_INDEXES),
    }
}

fn tls_slot(index: u32) -> Option<usize> {
    // SAFETY: one thread; nothing else holds it.
    let used = unsafe { *TLS_USED.get() };
    ((index as usize) < TLS_SLOTS && used & (1 << index) != 0).then_some(index as usize)
}

/// As on Windows, success also clears the last error, since a stored 0 and
/// a failure both return 0.
pub extern "win64" fn tls_get_value(index: u32) -> usize {
    match tls_slot(index) {
        Some(slot) => {
            set_last_error(0);
            // SAFETY: one thread; nothing else holds it.
            unsafe { TLS_VALUES.get()[slot] }
        }
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

pub extern "win64" fn tls_set_value(index: u32, value: usize) -> i32 {
    match tls_slot(index) {
        Some(slot) => {
            // SAFETY: one thread; nothing else holds it.
            unsafe { TLS_VALUES.get()[slot] = value };
            1
        }
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

pub extern "win64" fn tls_free(index: u32) -> i32 {
    match tls_slot(index) {
        Some(slot) => {
            // SAFETY: one thread; nothing else holds it.
            unsafe { *TLS_USED.get() &= !(1 << slot) };
            1
        }
        None => fail(ERROR_INVALID_PARAMETER, 0),
    }
}

// ---------------------------------------------------------------- critical sections

/// `CRITICAL_SECTION`, as Windows lays it out (40 bytes).
#[repr(C)]
pub struct CriticalSection {
    debug_info: usize,
    lock_count: i32,
    recursion_count: i32,
    owning_thread: usize,
    lock_semaphore: usize,
    spin_count: usize,
}

/// The one thread's id, as a critical section records its owner.
const THREAD: usize = 1;

/// # Safety
/// `section` points at a `CRITICAL_SECTION`, as the Windows contract says.
pub unsafe extern "win64" fn initialize_critical_section(section: *mut CriticalSection) {
    // SAFETY: the caller's promise.
    unsafe {
        section.write(CriticalSection {
            debug_info: 0,
            lock_count: -1,
            recursion_count: 0,
            owning_thread: 0,
            lock_semaphore: 0,
            spin_count: 0,
        })
    };
}

/// With one thread, the owner is always the caller, so entering never
/// waits: it counts, as a recursive entry on Windows does.
///
/// # Safety
/// As [`initialize_critical_section`], and initialised.
pub unsafe extern "win64" fn enter_critical_section(section: *mut CriticalSection) {
    // SAFETY: the caller's promise.
    let section = unsafe { &mut *section };
    section.lock_count += 1;
    section.recursion_count += 1;
    section.owning_thread = THREAD;
}

/// # Safety
/// As [`enter_critical_section`].
pub unsafe extern "win64" fn leave_critical_section(section: *mut CriticalSection) {
    // SAFETY: the caller's promise.
    let section = unsafe { &mut *section };
    if section.recursion_count > 0 {
        section.lock_count -= 1;
        section.recursion_count -= 1;
        if section.recursion_count == 0 {
            section.owning_thread = 0;
        }
    }
}

/// Nothing was allocated for it, so there is nothing to give back.
pub extern "win64" fn delete_critical_section(_section: *mut CriticalSection) {}

// ---------------------------------------------------------------- start-up information

/// `GetStartupInfoA`: nothing was asked of the window or the handles, so
/// every field is zero but the size.
///
/// # Safety
/// `info` points at a `STARTUPINFOA` (104 bytes).
pub unsafe extern "win64" fn get_startup_info_a(info: *mut u8) {
    // SAFETY: the caller's promise.
    unsafe {
        core::ptr::write_bytes(info, 0, 104);
        (info as *mut u32).write_unaligned(104);
    }
}

/// The handler a program sets for exceptions nobody else handles. Kept and
/// handed back, as Windows does; this system never calls it, because a
/// fault in ring 3 ends the task (DESIGN.md section 2).
static UNHANDLED_FILTER: One<usize> = One::new(0);

pub extern "win64" fn set_unhandled_exception_filter(filter: usize) -> usize {
    // SAFETY: one thread; nothing else holds it.
    core::mem::replace(unsafe { UNHANDLED_FILTER.get() }, filter)
}

// ---------------------------------------------------------------- memory protection

/// What memory is mapped where is the kernel's to know, and there is no
/// call to ask it yet, so this fails rather than guess.
pub extern "win64" fn virtual_query(_address: usize, _info: *mut u8, _len: usize) -> usize {
    fail(ERROR_INVALID_PARAMETER, 0)
}

/// Memory's protection never changes in ring 3 on CorrodeOS (no page is
/// ever both writable and executable, and none becomes either), so this
/// is refused.
pub extern "win64" fn virtual_protect(_address: usize, _len: usize, _new: u32, _old: *mut u32) -> i32 {
    fail(ERROR_ACCESS_DENIED, 0)
}

// ---------------------------------------------------------------- time

pub extern "win64" fn sleep(ms: u32) {
    if ms == u32::MAX {
        loop {
            corrode_rt::sleep_ns(u64::MAX / 2);
        }
    }
    corrode_rt::sleep_ns(ms as u64 * 1_000_000);
}

/// # Safety
/// `count` points at 8 writable bytes.
pub unsafe extern "win64" fn query_performance_counter(count: *mut i64) -> i32 {
    // SAFETY: the caller's promise.
    unsafe { count.write_unaligned(corrode_rt::now_ns() as i64) };
    1
}

/// Nanoseconds: the counter is the monotonic clock.
///
/// # Safety
/// `frequency` points at 8 writable bytes.
pub unsafe extern "win64" fn query_performance_frequency(frequency: *mut i64) -> i32 {
    // SAFETY: the caller's promise.
    unsafe { frequency.write_unaligned(1_000_000_000) };
    1
}

// ---------------------------------------------------------------- code pages

/// The "ANSI" code page is UTF-8 (65001), as Windows allows a program to
/// choose; so is the OEM one. `CP_ACP` (0), `CP_OEMCP` (1), `CP_THREAD_ACP`
/// (3) and `CP_UTF8` all mean it.
pub const CP_UTF8: u32 = 65001;

fn is_utf8(code_page: u32) -> bool {
    matches!(code_page, 0 | 1 | 3 | CP_UTF8)
}

pub extern "win64" fn get_acp() -> u32 {
    CP_UTF8
}

/// No code page here has lead bytes: UTF-8 is not a DBCS.
pub extern "win64" fn is_dbcs_lead_byte_ex(_code_page: u32, _byte: u8) -> i32 {
    0
}

/// The `len` units at `text`, or up to and including its zero if `len` is
/// -1.
///
/// # Safety
/// `text` holds `len` units, or a zero-ended string when `len` is -1.
unsafe fn units<'a, T: Copy + PartialEq + Default>(text: *const T, len: i32) -> &'a [T] {
    let len = if len < 0 {
        // SAFETY: the caller's promise: a zero ends it.
        (0..).find(|i| unsafe { text.add(*i).read_unaligned() } == T::default()).unwrap_or(0) + 1
    } else {
        len as usize
    };
    // SAFETY: the caller's promise.
    unsafe { core::slice::from_raw_parts(text, len) }
}

/// UTF-8 to UTF-16. `room` 0 asks how many units it takes.
///
/// # Safety
/// As the Windows contract says: `text` holds `len` bytes (or ends in a
/// zero, `len` -1), `out` holds `room` units.
pub unsafe extern "win64" fn multi_byte_to_wide_char(
    code_page: u32,
    flags: u32,
    text: *const u8,
    len: i32,
    out: *mut u16,
    room: i32,
) -> i32 {
    const MB_ERR_INVALID_CHARS: u32 = 0x8;
    if !is_utf8(code_page) || text.is_null() || len == 0 || room < 0 {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    // SAFETY: the caller's promise.
    let bytes = unsafe { units(text, len) };
    let mut written = 0usize;
    let mut put = |unit: u16| -> bool {
        if room != 0 {
            if written == room as usize {
                return false;
            }
            // SAFETY: the caller's promise; below `room`.
            unsafe { out.add(written).write_unaligned(unit) };
        }
        written += 1;
        true
    };
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            let mut buffer = [0u16; 2];
            for unit in character.encode_utf16(&mut buffer) {
                if !put(*unit) {
                    return fail(ERROR_INSUFFICIENT_BUFFER, 0);
                }
            }
        }
        if !chunk.invalid().is_empty() {
            if flags & MB_ERR_INVALID_CHARS != 0 {
                return fail(ERROR_NO_UNICODE_TRANSLATION, 0);
            }
            if !put(0xfffd) {
                return fail(ERROR_INSUFFICIENT_BUFFER, 0);
            }
        }
    }
    written as i32
}

/// UTF-16 to UTF-8. `room` 0 asks how many bytes it takes. For UTF-8,
/// Windows wants the default-character arguments null; an unpaired
/// surrogate becomes U+FFFD.
///
/// # Safety
/// As [`multi_byte_to_wide_char`], the other way.
pub unsafe extern "win64" fn wide_char_to_multi_byte(
    code_page: u32,
    _flags: u32,
    text: *const u16,
    len: i32,
    out: *mut u8,
    room: i32,
    default: *const u8,
    used_default: *mut i32,
) -> i32 {
    if !is_utf8(code_page) || text.is_null() || len == 0 || room < 0 {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    if !default.is_null() || !used_default.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    // SAFETY: the caller's promise.
    let wide = unsafe { units(text, len) };
    let mut written = 0usize;
    for character in char::decode_utf16(wide.iter().copied()) {
        let character = character.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut buffer = [0u8; 4];
        let encoded = character.encode_utf8(&mut buffer).as_bytes();
        if room != 0 {
            if written + encoded.len() > room as usize {
                return fail(ERROR_INSUFFICIENT_BUFFER, 0);
            }
            // SAFETY: the caller's promise; inside `room`.
            unsafe { core::ptr::copy_nonoverlapping(encoded.as_ptr(), out.add(written), encoded.len()) };
        }
        written += encoded.len();
    }
    written as i32
}

// ---------------------------------------------------------------- semaphores

/// A semaphore: a count, never above its most. With one thread, a wait
/// either takes a unit at once or can only time out: nothing else could
/// release one meanwhile, so an endless wait on an empty one never
/// returns, as on Windows.
#[derive(Clone, Copy)]
struct Semaphore {
    count: i32,
    most: i32,
}

const SEMAPHORES: usize = 64;
/// A semaphore's handle is this plus its slot, apart from files' (0x100)
/// and searches' (0x200).
const SEMAPHORE_HANDLES: isize = 0x300;
static SEMAPHORE_TABLE: One<[Option<Semaphore>; SEMAPHORES]> = One::new([None; SEMAPHORES]);

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_TOO_MANY_POSTS: u32 = 298;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_FAILED: u32 = u32::MAX;
const INFINITE: u32 = u32::MAX;

fn semaphore(handle: isize) -> Option<&'static mut Option<Semaphore>> {
    let slot = usize::try_from(handle.checked_sub(SEMAPHORE_HANDLES)?).ok()?;
    // SAFETY: one thread; no other reference is alive.
    unsafe { SEMAPHORE_TABLE.get() }.get_mut(slot)
}

/// Named semaphores would be shared between processes; there is no other
/// process to share with, so a name is refused rather than ignored.
pub extern "win64" fn create_semaphore_w(_attributes: usize, initial: i32, most: i32, name: *const u16) -> isize {
    if most <= 0 || initial < 0 || initial > most || !name.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    // SAFETY: one thread; no other reference is alive.
    let table = unsafe { SEMAPHORE_TABLE.get() };
    match table.iter().position(Option::is_none) {
        Some(slot) => {
            table[slot] = Some(Semaphore { count: initial, most });
            SEMAPHORE_HANDLES + slot as isize
        }
        None => fail(ERROR_NOT_ENOUGH_MEMORY, 0),
    }
}

/// # Safety
/// `previous` is null or writable.
pub unsafe extern "win64" fn release_semaphore(handle: isize, units: i32, previous: *mut i32) -> i32 {
    let Some(Some(semaphore)) = semaphore(handle) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    if units <= 0 {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    if semaphore.count.checked_add(units).is_none_or(|count| count > semaphore.most) {
        return fail(ERROR_TOO_MANY_POSTS, 0);
    }
    if !previous.is_null() {
        // SAFETY: the caller's promise.
        unsafe { previous.write_unaligned(semaphore.count) };
    }
    semaphore.count += units;
    1
}

pub extern "win64" fn wait_for_single_object(handle: isize, timeout: u32) -> u32 {
    let Some(Some(semaphore)) = semaphore(handle) else {
        return fail(ERROR_INVALID_HANDLE, WAIT_FAILED);
    };
    if semaphore.count > 0 {
        semaphore.count -= 1;
        return WAIT_OBJECT_0;
    }
    sleep(timeout);
    if timeout == INFINITE {
        unreachable!()
    }
    WAIT_TIMEOUT
}

/// `CloseHandle` for a semaphore: `None` if `handle` is not one.
pub fn close_semaphore(handle: isize) -> Option<i32> {
    let entry = semaphore(handle)?;
    Some(if entry.take().is_some() { 1 } else { fail(ERROR_INVALID_HANDLE, 0) })
}
