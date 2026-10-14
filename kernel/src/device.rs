//! Device memory in a task's address space.
//!
//! A driver in ring 3 needs its device's registers, and the syscall boundary
//! takes no pointers. So this is [`crate::surface::map`] again with a
//! different object behind the capability: the task says "put this in my
//! address space" and the kernel says where it went and how long it is.
//!
//! The address depends only on which region it is, so mapping twice is
//! harmless and gives the same answer, and no task ever names a physical
//! address. What a task can reach is exactly the BARs it holds capabilities
//! to -- which for a task holding none is nothing, the same as before there
//! was any of this.
//!
//! # What is held back
//!
//! If a device keeps its MSI-X table behind a BAR, the pages holding that
//! table are not mapped. The table is where each interrupt's destination is
//! written; a driver that could write it could aim a message at any vector
//! on any CPU. The kernel programs it, in [`crate::irq`], and the driver
//! finds a hole there.

use crate::arch::x86_64;
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::paging;
use crate::pci;
use crate::task;

const PAGE: u64 = 4096;

/// Where device memory is mapped in a task: a terabyte up, far above its
/// code, its stack and any surface.
pub const USER_BASE: u64 = 0x0000_0100_0000_0000;

/// Address space set aside for each region, and so the largest BAR that can
/// be mapped at all: 4 GiB.
const SPAN: u64 = 4 << 30;

/// Regions with a place in a task. The table would have to hold more
/// devices than a machine has slots before this mattered.
const REGIONS: usize = 1024;

/// Where a region goes in the lower half of any task that maps it.
pub fn user_address(index: usize) -> u64 {
    USER_BASE + index as u64 * SPAN
}

/// Where a region is in the task that asked, and how much of it there is.
pub struct Mapped {
    pub address: u64,
    pub len: u64,
}

/// Map the device memory in `slot` into the calling task. Requires READ; the
/// mapping is writable only if the capability also carries WRITE.
///
/// Uncached and never executable. A register read has to reach the device
/// every time, and a device is not somewhere to run code from.
pub fn map(slot: CapPtr) -> Result<Mapped, CapError> {
    let (index, rights) = cap::device_memory_of(slot, Rights::READ)?;
    let region = pci::region(index).ok_or(CapError::Stale)?;
    if index >= REGIONS || region.len > SPAN {
        return Err(CapError::NoSpace);
    }
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    // A kernel task has no lower half to put it in.
    let mut space = task::space_of(current).ok_or(CapError::NoSpace)?;

    let mut flags = paging::USER | paging::NO_EXECUTE | paging::NO_CACHE | paging::WRITE_THROUGH;
    if rights.contains(Rights::WRITE) {
        flags |= paging::WRITABLE;
    }

    // Whole pages the kernel keeps: everything the MSI-X table touches.
    let held = region
        .reserved
        .map(|(offset, len)| (offset / PAGE * PAGE, (offset + len).div_ceil(PAGE) * PAGE));

    let address = user_address(index);
    // A BAR smaller than a page still occupies one: the device decodes its
    // few bytes and the rest of the page is whatever is next to it on the
    // bus, which is why the firmware aligns small BARs to a page of their own.
    let pages = region.len.div_ceil(PAGE);
    for page in 0..pages {
        let offset = page * PAGE;
        if held.is_some_and(|(start, end)| offset >= start && offset < end) {
            continue;
        }
        let va = address + offset;
        if !space.map_4k(va, region.physical + offset, flags) {
            return Err(CapError::NoSpace);
        }
        // The entry may have been there already with other permissions. This
        // space is installed on this CPU and no other, so one flush is all.
        x86_64::flush_page(va);
    }

    Ok(Mapped {
        address,
        len: region.len,
    })
}
