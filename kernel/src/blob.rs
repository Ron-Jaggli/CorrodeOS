//! Blobs: a file from the boot medium, handed to one task to read.
//!
//! Firmware is the case this exists for. A GPU driver needs tens of
//! megabytes of signed vendor code that it does not interpret, only loads,
//! and the design rules say where that comes from: the boot medium, read by
//! the bootloader before the kernel's first instruction, so nothing waits on
//! a disk once the system is running. A Limine module is exactly that.
//!
//! What the kernel adds is who may see it. A **blob** capability names one
//! module. READ maps it into the holder, read-only and never executable,
//! whatever else the capability carries -- the file is the bootloader's
//! copy, shared by every holder, and nobody gets to change it under anyone
//! else. The frames mapped are the module's own: tens of megabytes are not
//! copied to be looked at.
//!
//! A blob gives up no physical addresses. A driver that needs its device to
//! read firmware by DMA copies it into memory it holds with WRITE, and tells
//! the device about that -- so the device never learns where the shared copy
//! is, and cannot be made to write over it.

use crate::arch::x86_64;
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::paging;
use crate::modules;
use crate::task;

const PAGE: u64 = 4096;

/// Address space set aside for each blob, and so the largest one: 1 GiB.
const SPAN: u64 = 1 << 30;

/// Where blobs go in a task: three terabytes up, past memory objects.
pub const USER_BASE: u64 = 0x0000_0300_0000_0000;

/// Modules there can be blobs of.
const BLOBS: usize = 128;

pub fn user_address(index: usize) -> u64 {
    USER_BASE + index as u64 * SPAN
}

/// Where a blob is in the task that asked, and how many bytes it is.
pub struct Mapped {
    pub address: u64,
    pub len: u64,
}

/// Map the blob in `slot` into the calling task. Requires READ. Read-only
/// and never executable, whatever rights the capability carries.
pub fn map(slot: CapPtr) -> Result<Mapped, CapError> {
    let index = cap::blob_of(slot, Rights::READ)?;
    let module = modules::get(index).ok_or(CapError::Stale)?;
    if index >= BLOBS || module.bytes.len() as u64 > SPAN {
        return Err(CapError::NoSpace);
    }
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    let mut space = task::space_of(current).ok_or(CapError::NoSpace)?;

    let address = user_address(index);
    let pages = (module.bytes.len() as u64).div_ceil(PAGE);
    for page in 0..pages {
        let va = address + page * PAGE;
        // The module starts on a page boundary, so this is its own frame. The
        // tail of the last page is whatever the bootloader left after the
        // file, and the length returned says where the file ends.
        if !space.map_4k(
            va,
            module.physical + page * PAGE,
            paging::USER | paging::NO_EXECUTE,
        ) {
            return Err(CapError::NoSpace);
        }
        x86_64::flush_page(va);
    }
    Ok(Mapped {
        address,
        len: module.bytes.len() as u64,
    })
}
