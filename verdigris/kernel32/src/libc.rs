//! msvcrt's string, number, sorting, character, random and time functions:
//! what `string.h`, `stdlib.h`, `ctype.h` and `time.h` give a console
//! program, apart from its streams (in `msvcrt.rs`). Written from the C
//! standard and Microsoft's documentation; no C runtime's code is in it.
//!
//! The "C" locale throughout: the character classes are ASCII's, and a
//! byte above 127 is in none of them. `long` is 32 bits, as on Windows.

use core::ffi::c_void;

use crate::format::{format, Args};
use crate::msvcrt::{errno, strlen, strncmp};
use crate::text::{number, to_long, to_unsigned_long};
use crate::One;

const ERANGE: i32 = 34;

// ---------------------------------------------------------------- strings

/// # Safety
/// `to` holds a zero-ended string with room for `from` after it.
pub unsafe extern "win64" fn strcat(to: *mut u8, from: *const u8) -> *mut u8 {
    // SAFETY: the caller's promise.
    unsafe {
        let at = to.add(strlen(to));
        core::ptr::copy(from, at, strlen(from) + 1);
    }
    to
}

/// `strncpy`: at most `len` bytes of `from`, and zeros after it up to
/// `len`. If `from` is longer, no zero ends `to`.
///
/// # Safety
/// `to` has room for `len` bytes; `from` is zero-ended or `len` long.
pub unsafe extern "win64" fn strncpy(to: *mut u8, from: *const u8, len: usize) -> *mut u8 {
    let mut ended = false;
    for i in 0..len {
        // SAFETY: the caller's promise; `from` is not read past its zero.
        unsafe {
            let byte = if ended { 0 } else { from.add(i).read() };
            ended |= byte == 0;
            to.add(i).write(byte);
        }
    }
    to
}

/// `strchr`: the first `c` (as a byte) in `text`, its ending zero included,
/// or null.
///
/// # Safety
/// A zero-ended string.
pub unsafe extern "win64" fn strchr(text: *const u8, c: i32) -> *mut u8 {
    let want = c as u8;
    let mut at = text;
    loop {
        // SAFETY: the caller's promise; read up to the zero.
        let byte = unsafe { at.read() };
        if byte == want {
            return at.cast_mut();
        }
        if byte == 0 {
            return core::ptr::null_mut();
        }
        // SAFETY: not past the zero.
        at = unsafe { at.add(1) };
    }
}

/// `strrchr`: the last `c` in `text`, its ending zero included, or null.
///
/// # Safety
/// A zero-ended string.
pub unsafe extern "win64" fn strrchr(text: *const u8, c: i32) -> *mut u8 {
    let want = c as u8;
    // SAFETY: the caller's promise.
    let len = unsafe { strlen(text) };
    (0..=len)
        .rev()
        // SAFETY: inside the string and its zero.
        .find(|i| unsafe { text.add(*i).read() } == want)
        // SAFETY: as above.
        .map_or(core::ptr::null_mut(), |i| unsafe { text.add(i) }.cast_mut())
}

/// `strstr`: where `needle` first appears in `haystack`; `haystack` itself
/// for an empty needle; null if it does not.
///
/// # Safety
/// Two zero-ended strings.
pub unsafe extern "win64" fn strstr(haystack: *const u8, needle: *const u8) -> *mut u8 {
    // SAFETY: the caller's promise.
    let (len, want) = unsafe { (strlen(haystack), strlen(needle)) };
    if want > len {
        return core::ptr::null_mut();
    }
    for i in 0..=len - want {
        // SAFETY: `want` bytes from `i` are inside the haystack.
        if unsafe { strncmp(haystack.add(i), needle, want) } == 0 {
            // SAFETY: as above.
            return unsafe { haystack.add(i) }.cast_mut();
        }
    }
    core::ptr::null_mut()
}

/// # Safety
/// `len` readable bytes at `memory`.
pub unsafe extern "win64" fn memchr(memory: *const u8, c: i32, len: usize) -> *mut u8 {
    let want = c as u8;
    for i in 0..len {
        // SAFETY: the caller's promise.
        if unsafe { memory.add(i).read() } == want {
            // SAFETY: as above.
            return unsafe { memory.add(i) }.cast_mut();
        }
    }
    core::ptr::null_mut()
}

// ---------------------------------------------------------------- numbers

/// The bytes of a zero-ended string, without its zero.
///
/// # Safety
/// A zero-ended string.
unsafe fn bytes<'a>(text: *const u8) -> &'a [u8] {
    // SAFETY: the caller's promise.
    unsafe { core::slice::from_raw_parts(text, strlen(text)) }
}

fn set_errno(value: i32) {
    // SAFETY: one thread; `errno` is the runtime's own variable.
    unsafe { *errno() = value };
}

/// # Safety
/// `text` is zero-ended; `end` is null or writable.
unsafe fn end_at(text: *const u8, len: usize, end: *mut *mut u8) {
    if !end.is_null() {
        // SAFETY: the caller's promise; `len` is inside the string.
        unsafe { end.write_unaligned(text.add(len).cast_mut()) };
    }
}

/// `strtol`, for a `long` of 32 bits. Out of range is `LONG_MAX` or
/// `LONG_MIN`, with `errno` `ERANGE`.
///
/// # Safety
/// `text` is zero-ended; `end` is null or writable.
pub unsafe extern "win64" fn strtol(text: *const u8, end: *mut *mut u8, base: i32) -> i32 {
    // SAFETY: the caller's promise.
    let read = number(unsafe { bytes(text) }, base as u32);
    // SAFETY: as above.
    unsafe { end_at(text, read.len, end) };
    let (value, out_of_range) = to_long(&read);
    if out_of_range {
        set_errno(ERANGE);
    }
    value
}

/// `strtoul`, for an `unsigned long` of 32 bits. A minus sign negates, so
/// "-1" is `ULONG_MAX`; out of range is `ULONG_MAX` with `ERANGE`.
///
/// # Safety
/// As `strtol`.
pub unsafe extern "win64" fn strtoul(text: *const u8, end: *mut *mut u8, base: i32) -> u32 {
    // SAFETY: the caller's promise.
    let read = number(unsafe { bytes(text) }, base as u32);
    // SAFETY: as above.
    unsafe { end_at(text, read.len, end) };
    let (value, out_of_range) = to_unsigned_long(&read);
    if out_of_range {
        set_errno(ERANGE);
    }
    value
}

/// `atoi`: `strtol` in base 10, without its `errno`.
///
/// # Safety
/// A zero-ended string.
pub unsafe extern "win64" fn atoi(text: *const u8) -> i32 {
    // SAFETY: the caller's promise.
    to_long(&number(unsafe { bytes(text) }, 10)).0
}

// ---------------------------------------------------------------- sorting

type Compare = extern "win64" fn(*const c_void, *const c_void) -> i32;

/// # Safety
/// Two elements of `size` bytes, not overlapping unless the same.
unsafe fn swap(a: *mut u8, b: *mut u8, size: usize) {
    if a != b {
        // SAFETY: the caller's promise.
        unsafe { core::ptr::swap_nonoverlapping(a, b, size) };
    }
}

/// `qsort`: heapsort, in place. It never allocates, and a comparison
/// function that is not a consistent order leaves the elements in some
/// order rather than failing.
///
/// # Safety
/// `count` elements of `size` bytes at `base`; `compare` is a function
/// taking two of them.
pub unsafe extern "win64" fn qsort(base: *mut c_void, count: usize, size: usize, compare: Compare) {
    if count < 2 || size == 0 {
        return;
    }
    let base = base.cast::<u8>();
    // SAFETY: the caller's promise; every index used is below `count`.
    let at = |i: usize| unsafe { base.add(i * size) };
    let less = |i: usize, j: usize| compare(at(i).cast(), at(j).cast()) < 0;
    // Move element `root` down the heap of the first `len` elements.
    let sift = |mut root: usize, len: usize| loop {
        let mut child = 2 * root + 1;
        if child >= len {
            break;
        }
        if child + 1 < len && less(child, child + 1) {
            child += 1;
        }
        if !less(root, child) {
            break;
        }
        // SAFETY: two elements of the array.
        unsafe { swap(at(root), at(child), size) };
        root = child;
    };
    for root in (0..count / 2).rev() {
        sift(root, count);
    }
    for last in (1..count).rev() {
        // SAFETY: two elements of the array.
        unsafe { swap(at(0), at(last), size) };
        sift(0, last);
    }
}

/// `bsearch`: an element equal to `key` in a sorted array, or null.
///
/// # Safety
/// As `qsort`; `key` is what `compare` takes as its first argument.
pub unsafe extern "win64" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    count: usize,
    size: usize,
    compare: Compare,
) -> *mut c_void {
    let (mut low, mut high) = (0, count);
    while low < high {
        let middle = low + (high - low) / 2;
        // SAFETY: `middle` is below `count`.
        let element = unsafe { base.cast::<u8>().add(middle * size) };
        match compare(key, element.cast()) {
            0 => return element.cast_mut().cast(),
            order if order < 0 => high = middle,
            _ => low = middle + 1,
        }
    }
    core::ptr::null_mut()
}

// ---------------------------------------------------------------- the rest of stdlib

/// `getenv`: a Windows program here has no environment, so nothing is set.
pub extern "win64" fn getenv(_name: *const u8) -> *mut u8 {
    core::ptr::null_mut()
}

/// msvcrt's generator, so a seed gives the sequence it gives on Windows.
static SEED: One<u32> = One::new(1);

pub extern "win64" fn srand(seed: u32) {
    // SAFETY: one thread.
    unsafe { *SEED.get() = seed };
}

/// `rand`: 0 to `RAND_MAX` (32767).
pub extern "win64" fn rand() -> i32 {
    // SAFETY: one thread.
    let seed = unsafe { SEED.get() };
    *seed = seed.wrapping_mul(214_013).wrapping_add(2_531_011);
    ((*seed >> 16) & 0x7fff) as i32
}

/// `_vscprintf`: how many bytes `vsprintf` would write, its zero not
/// counted. mingw's `snprintf` asks it for C99's return value.
///
/// # Safety
/// As `vsprintf`.
pub unsafe extern "win64" fn vscprintf(text: *const u8, args: *const u64) -> i32 {
    // SAFETY: the caller's promise.
    let count = unsafe { format(text, &mut Args(args), &mut |_| {}) };
    i32::try_from(count).unwrap_or(-1)
}

// ---------------------------------------------------------------- ctype

/// The class of `c` as `ctype.h` takes it: an `unsigned char`'s value or
/// `EOF`. Anything else (and `EOF`) is in no class.
fn ascii(c: i32) -> Option<u8> {
    u8::try_from(c).ok().filter(u8::is_ascii)
}

macro_rules! class {
    ($($name:ident: $test:expr;)*) => {$(
        pub extern "win64" fn $name(c: i32) -> i32 {
            ascii(c).is_some_and($test) as i32
        }
    )*};
}

class! {
    isalnum: |b: u8| b.is_ascii_alphanumeric();
    isalpha: |b: u8| b.is_ascii_alphabetic();
    isdigit: |b: u8| b.is_ascii_digit();
    islower: |b: u8| b.is_ascii_lowercase();
    isupper: |b: u8| b.is_ascii_uppercase();
    isxdigit: |b: u8| b.is_ascii_hexdigit();
    ispunct: |b: u8| b.is_ascii_punctuation();
    isspace: crate::text::is_space;
}

pub extern "win64" fn toupper(c: i32) -> i32 {
    ascii(c).map_or(c, |b| b.to_ascii_uppercase() as i32)
}

pub extern "win64" fn tolower(c: i32) -> i32 {
    ascii(c).map_or(c, |b| b.to_ascii_lowercase() as i32)
}

// ---------------------------------------------------------------- time

/// `_time64` (`time`): seconds since 1970, also stored at `out` if it is
/// not null.
///
/// # Safety
/// `out` is null or writable.
pub unsafe extern "win64" fn time64(out: *mut i64) -> i64 {
    let now = crate::system::unix_seconds();
    if !out.is_null() {
        // SAFETY: the caller's promise.
        unsafe { out.write_unaligned(now) };
    }
    now
}

/// `clock`: milliseconds (`CLOCKS_PER_SEC` is 1000 on Windows) since the
/// program started. Its time is the time it has existed, not its time on a
/// processor, which is what msvcrt's `clock` measures too.
pub extern "win64" fn clock() -> i32 {
    let ms = (corrode_rt::now_ns().saturating_sub(crate::system::started_ns())) / 1_000_000;
    i32::try_from(ms).unwrap_or(i32::MAX)
}
