//! Sealing, for ring 3: what a sealer capability lets its holder ask of the
//! secure core.
//!
//! A sealer ([`cap::Object::Sealer`]) names a holder, and the secure core
//! keeps a key for each holder that never leaves it (see [`secure_core`]).
//! Bootstrap makes one sealer for Settings and another for the lock screen,
//! so neither can open what the other sealed, and nothing else holds one.
//!
//! The message travels in the first page of a memory capability the caller
//! holds, never by a pointer it names: the kernel finds the page through the
//! capability and copies it once, in or out, so what the caller does to the
//! page meanwhile changes nothing the kernel checked.
//!
//! Sealing gives confidentiality and integrity for the length of one boot:
//! the key is new every boot, so nothing sealed survives a restart. That is
//! why nothing is written to disk sealed.

use alloc::vec;

use corrode_abi::seal::{MOST, OVERHEAD};
use corrode_seal::{NONCE, TAG};

use crate::cap::{self, CapError, CapPtr, Rights};
use crate::limine;
use crate::memory;
use crate::secure_core::{self, CallError, STATUS_FORGED};

/// The holders. Zero is no one's, and the secure core refuses it.
pub const SETTINGS: u32 = 1;
pub const LOCK: u32 = 2;

/// Seal `len` bytes from byte [`OVERHEAD`] of the first page of `memory`,
/// for the holder `sealer` names. The nonce, the tag and the ciphertext
/// replace them from byte 0. Returns the sealed length.
pub fn seal(sealer: CapPtr, memory: CapPtr, len: u64) -> Result<u64, CapError> {
    let holder = cap::sealer_of(sealer, Rights::WRITE)?;
    let len = usize::try_from(len).map_err(|_| CapError::Invalid)?;
    if len > MOST {
        return Err(CapError::Invalid);
    }
    let page = page(memory)?;

    let mut data = vec![0u8; len];
    // SAFETY: `page` is a whole frame of the caller's memory, mapped by the
    // HHDM, and OVERHEAD + MOST is less than a page.
    unsafe { copy_out(page, OVERHEAD, &mut data) };
    let (nonce, tag) = match secure_core::seal(holder, &mut data) {
        Ok(answer) => answer,
        Err(error) => {
            // Still the caller's plaintext: not left in the kernel's heap.
            wipe(&mut data);
            return Err(refusal(error));
        }
    };

    let mut sealed = vec![0u8; OVERHEAD + len];
    sealed[..NONCE].copy_from_slice(&nonce);
    sealed[NONCE..OVERHEAD].copy_from_slice(&tag);
    sealed[OVERHEAD..].copy_from_slice(&data);
    // SAFETY: as above.
    unsafe { copy_in(page, 0, &sealed) };
    Ok(sealed.len() as u64)
}

/// Open the `len`-byte sealed message at byte 0 of the first page of
/// `memory`. What was sealed is written back from byte [`OVERHEAD`]; its
/// length is returned. A message that does not check out is refused with
/// [`CapError::Forged`], and its bytes in the page are zeroed.
pub fn unseal(sealer: CapPtr, memory: CapPtr, len: u64) -> Result<u64, CapError> {
    let holder = cap::sealer_of(sealer, Rights::READ)?;
    let len = usize::try_from(len).map_err(|_| CapError::Invalid)?;
    if !(OVERHEAD..=OVERHEAD + MOST).contains(&len) {
        return Err(CapError::Invalid);
    }
    let page = page(memory)?;

    let mut sealed = vec![0u8; len];
    // SAFETY: `page` is a whole frame of the caller's memory, and len is at
    // most OVERHEAD + MOST, less than a page.
    unsafe { copy_out(page, 0, &mut sealed) };
    let nonce: [u8; NONCE] = sealed[..NONCE].try_into().unwrap_or_default();
    let tag: [u8; TAG] = sealed[NONCE..OVERHEAD].try_into().unwrap_or_default();
    let data = &mut sealed[OVERHEAD..];
    let result = match secure_core::unseal(holder, &nonce, &tag, data) {
        Ok(()) => {
            // SAFETY: as above.
            unsafe { copy_in(page, OVERHEAD, data) };
            Ok(data.len() as u64)
        }
        Err(error) => {
            if error == CallError::Refused(STATUS_FORGED) {
                // SAFETY: as above.
                unsafe { copy_in(page, 0, &vec![0u8; len]) };
            }
            Err(refusal(error))
        }
    };
    // What was opened is the caller's again, and not the kernel heap's.
    wipe(&mut sealed);
    result
}

/// Zero bytes about to be freed. Volatile, because zeroing memory that is
/// freed straight after is a store the compiler may otherwise drop.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: a valid, exclusive reference.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }
}

fn refusal(error: CallError) -> CapError {
    match error {
        CallError::Refused(STATUS_FORGED) => CapError::Forged,
        CallError::NotRunning | CallError::NoAnswer => CapError::Unavailable,
        CallError::TooLong | CallError::Refused(_) => CapError::Invalid,
    }
}

/// The kernel's address for the first page of the memory in `slot`, which
/// must be held READ and WRITE: the call reads the page and writes it back.
pub fn page(slot: CapPtr) -> Result<*mut u8, CapError> {
    let (index, _) = cap::memory_of(slot, Rights::READ_WRITE)?;
    let frame = memory::frame_of(index, 0).ok_or(CapError::Stale)?;
    let hhdm = limine::HHDM.response.get().ok_or(CapError::NoSpace)?.offset;
    Ok((frame + hhdm) as *mut u8)
}

/// # Safety
/// `page` is a 4 KiB frame mapped writable, and `at + out.len()` fits in it.
unsafe fn copy_out(page: *mut u8, at: usize, out: &mut [u8]) {
    // SAFETY: forwarded. The caller may be writing the page from another CPU
    // as this reads it; the bytes copied are then whatever they were, and
    // everything after works on this copy alone.
    unsafe { core::ptr::copy_nonoverlapping(page.add(at), out.as_mut_ptr(), out.len()) };
}

/// # Safety
/// As [`copy_out`].
unsafe fn copy_in(page: *mut u8, at: usize, from: &[u8]) {
    // SAFETY: forwarded.
    unsafe { core::ptr::copy_nonoverlapping(from.as_ptr(), page.add(at), from.len()) };
}
