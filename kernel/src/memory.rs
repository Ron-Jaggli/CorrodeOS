//! Memory: frames a task can map, and can tell a device about.
//!
//! A driver needs memory its device can reach on its own -- queues, command
//! buffers, the pixels a display controller scans out -- and that means
//! knowing physical addresses, which nothing else in the system is ever
//! told. So that knowledge is an operation on a capability, like everything
//! else: a **memory** capability names a run of frames, READ maps them,
//! WRITE makes the mapping writable and lets the holder ask where each page
//! physically is.
//!
//! The asymmetry is the point. A physical address is only good for one
//! thing, having a device write there, so asking for one needs the right to
//! write. And it is only ever the address of a frame the capability already
//! covers: a task cannot ask "what is at physical address X", only "where
//! is page N of the memory I hold".
//!
//! # Not contiguous
//!
//! The frames are whatever the allocator hands out, one at a time, and need
//! not be next to each other. Every structure the drivers here give a device
//! fits in a page or is described to it page by page, which is why that is
//! enough. A device that insists on more than a page of contiguous memory
//! needs a different allocator, and will get one when it shows up.
//!
//! # What is not here yet
//!
//! Memory is made by the kernel at boot for the driver that will use it,
//! and never given back. There is no way for a task to ask for more.

use alloc::vec::Vec;

use crate::arch::x86_64::{self, without_interrupts};
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::{frame, heap, paging};
use crate::sync::SpinLock;
use crate::task;

const PAGE: u64 = 4096;

/// Address space set aside for each object, in a task and in the kernel:
/// also the most one can hold.
const SPAN: u64 = 1 << 30;

/// Where memory goes in a task: two terabytes up, past device memory.
pub const USER_BASE: u64 = 0x0000_0200_0000_0000;

/// Where the kernel keeps its own view of memory it shares with a task:
/// four gigabytes above the heap, past the back buffer and the surfaces, in
/// the same PML4 slot so every address space has it.
const KERNEL_BASE: u64 = heap::HEAP_START as u64 + (4 << 30);

/// Objects there can be. Each takes a gigabyte of address space here and in
/// every task that maps it, and a PML4 slot has 512 of those.
const OBJECTS: usize = 256;

/// Frames that must be left over after an object has taken its share.
const RESERVE: usize = 1024;

struct Object {
    frames: Vec<u64>,
    /// Whether the kernel mapped it too, at [`kernel_address`].
    in_kernel: bool,
}

/// Locked with interrupts off, as everything a system call reaches is.
static TABLE: SpinLock<Vec<Object>> = SpinLock::new(Vec::new());

fn kernel_address(index: usize) -> u64 {
    KERNEL_BASE + index as u64 * SPAN
}

/// Where an object goes in the lower half of any task that maps it.
pub fn user_address(index: usize) -> u64 {
    USER_BASE + index as u64 * SPAN
}

/// Make `pages` of zeroed memory and return its number.
///
/// With `in_kernel`, the kernel maps it as well, writable, for memory it
/// shares with the task that will hold it. `None` if it is too big, there
/// are no numbers left, or frames are short. Like every `create`, this makes
/// an object and no authority over it.
pub fn create(pages: usize, in_kernel: bool) -> Option<usize> {
    if pages == 0 || pages as u64 * PAGE > SPAN {
        return None;
    }
    // Checked up front: running out half way would strand what was taken.
    let (free, _) = frame::stats();
    if free < pages + pages / 512 + 2 + RESERVE {
        return None;
    }

    // The number first, so a kernel mapping has its address.
    let index = without_interrupts(|| {
        let mut table = TABLE.lock();
        if table.len() >= OBJECTS {
            return None;
        }
        table.push(Object {
            frames: Vec::new(),
            in_kernel,
        });
        Some(table.len() - 1)
    })?;

    let mut frames = Vec::new();
    frames.try_reserve_exact(pages).ok()?;
    for page in 0..pages as u64 {
        let frame = frame::alloc()?;
        let ok = paging::with_kernel_mapper(|mapper| {
            // SAFETY: fresh from the allocator, so ours alone and covered by
            // the direct map. Wiped because it is about to be handed out.
            unsafe { mapper.zero_frame(frame.addr()) };
            !in_kernel
                || mapper.map_4k(
                    kernel_address(index) + page * PAGE,
                    frame.addr(),
                    paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
                )
        });
        if ok != Some(true) {
            return None;
        }
        frames.push(frame.addr());
    }

    without_interrupts(|| TABLE.lock()[index].frames = frames);
    Some(index)
}

/// The kernel's view of an object it shares, and its length in bytes.
/// `None` if it has none.
pub fn in_kernel(index: usize) -> Option<(*mut u8, usize)> {
    without_interrupts(|| {
        let table = TABLE.lock();
        let object = table.get(index)?;
        (object.in_kernel && !object.frames.is_empty()).then_some((
            kernel_address(index) as *mut u8,
            object.frames.len() * PAGE as usize,
        ))
    })
}

/// The frame behind one page of an object.
pub fn frame_of(index: usize, page: usize) -> Option<u64> {
    without_interrupts(|| TABLE.lock().get(index)?.frames.get(page).copied())
}

/// Where memory is in the task that asked, and how long.
pub struct Mapped {
    pub address: u64,
    pub len: u64,
}

/// Map the memory in `slot` into the calling task. Requires READ; writable
/// only if the capability also carries WRITE. Never executable.
pub fn map(slot: CapPtr) -> Result<Mapped, CapError> {
    let (index, rights) = cap::memory_of(slot, Rights::READ)?;
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    let mut space = task::space_of(current).ok_or(CapError::NoSpace)?;

    let mut flags = paging::USER | paging::NO_EXECUTE;
    if rights.contains(Rights::WRITE) {
        flags |= paging::WRITABLE;
    }
    let address = user_address(index);

    without_interrupts(|| {
        let table = TABLE.lock();
        let object = table.get(index).ok_or(CapError::Stale)?;
        for (page, frame) in object.frames.iter().enumerate() {
            let va = address + page as u64 * PAGE;
            if !space.map_4k(va, *frame, flags) {
                return Err(CapError::NoSpace);
            }
            x86_64::flush_page(va);
        }
        Ok(Mapped {
            address,
            len: object.frames.len() as u64 * PAGE,
        })
    })
}

/// Where page `page` of the memory in `slot` is physically. Requires WRITE:
/// the only use of a physical address is to have a device write there.
pub fn physical(slot: CapPtr, page: u64) -> Result<u64, CapError> {
    let (index, _) = cap::memory_of(slot, Rights::WRITE)?;
    let page = usize::try_from(page).map_err(|_| CapError::Invalid)?;
    without_interrupts(|| {
        let table = TABLE.lock();
        let object = table.get(index).ok_or(CapError::Stale)?;
        object.frames.get(page).copied().ok_or(CapError::Invalid)
    })
}
