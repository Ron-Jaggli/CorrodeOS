//! Physical frame allocator.
//!
//! The free list is stitched through the free frames themselves: each free
//! frame's first 8 bytes hold the physical address of the next one, reached via
//! Limine's higher-half direct map. That costs zero bytes of separate metadata
//! -- no bitmap sized to RAM, no bootstrap allocation problem -- and makes both
//! alloc and free O(1).
//!
//! The tradeoff is that a use-after-free corrupts the list itself rather than
//! just handing back a bad frame, which is why `free` is `unsafe` and carries a
//! real contract.

use crate::arch::x86_64::without_interrupts;
use crate::limine::{MemmapEntry, MEMMAP_USABLE};
use crate::sync::SpinLock;

pub const FRAME_SIZE: u64 = 4096;

/// A 4 KiB physical frame, identified by its base address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame(u64);

impl Frame {
    pub const fn addr(self) -> u64 {
        self.0
    }
}

pub struct FrameAllocator {
    /// Physical address of the first free frame, or 0 for empty.
    ///
    /// Address 0 doubles as the null sentinel, which is safe because frame 0
    /// holds the real-mode interrupt vector table and is never allocatable.
    head: u64,
    free: usize,
    total: usize,
    hhdm: u64,
}

impl FrameAllocator {
    pub const fn empty() -> Self {
        Self {
            head: 0,
            free: 0,
            total: 0,
            hhdm: 0,
        }
    }

    /// Push a frame onto the free list.
    ///
    /// # Safety
    /// `pa` must be the base of an unused, page-aligned 4 KiB region of real
    /// RAM that is mapped by the HHDM, and must not already be on the list.
    unsafe fn push(&mut self, pa: u64) {
        let slot = (pa + self.hhdm) as *mut u64;
        // SAFETY: the caller guarantees `pa` is usable RAM covered by the HHDM,
        // so `pa + hhdm` is a valid writable address, and 4 KiB alignment
        // trivially satisfies u64 alignment.
        unsafe { slot.write(self.head) };
        self.head = pa;
        self.free += 1;
    }

    pub fn alloc(&mut self) -> Option<Frame> {
        if self.head == 0 {
            return None;
        }
        let pa = self.head;
        let slot = (pa + self.hhdm) as *const u64;
        // SAFETY: `pa` came off our own free list, so it is a frame we
        // previously pushed: mapped, aligned, and holding the next link.
        self.head = unsafe { slot.read() };
        self.free -= 1;
        Some(Frame(pa))
    }

    /// Return a frame to the free list.
    ///
    /// # Safety
    /// `frame` must have come from `alloc`, must not already be free, and must
    /// have no live references or mappings pointing into it. Double-freeing
    /// builds a cycle in the list, which turns into an infinite loop or aliased
    /// allocations later rather than an immediate crash.
    pub unsafe fn free(&mut self, frame: Frame) {
        // SAFETY: the caller's contract says this frame is allocated and
        // unreferenced, which is exactly `push`'s requirement.
        unsafe { self.push(frame.addr()) };
    }

    pub fn free_frames(&self) -> usize {
        self.free
    }

    pub fn total_frames(&self) -> usize {
        self.total
    }
}

static ALLOCATOR: SpinLock<FrameAllocator> = SpinLock::new(FrameAllocator::empty());

/// Build the free list from Limine's memory map.
///
/// Only `MEMMAP_USABLE` regions are taken. Notably that excludes the kernel
/// image and the bootloader-reclaimable region holding the very structures we
/// are reading, so nothing here can pull the floor out from under itself.
pub fn init(hhdm: u64, entries: impl Iterator<Item = &'static MemmapEntry>) {
    let mut allocator = ALLOCATOR.lock();
    allocator.hhdm = hhdm;

    for entry in entries {
        if entry.kind != MEMMAP_USABLE {
            continue;
        }

        // Round inward: a partial frame at either end is not a frame.
        let start = entry.base.div_ceil(FRAME_SIZE) * FRAME_SIZE;
        let end = (entry.base + entry.length) / FRAME_SIZE * FRAME_SIZE;

        let mut pa = start;
        while pa + FRAME_SIZE <= end {
            // Frame 0 is the real-mode IVT and is our null sentinel; never
            // hand it out.
            if pa != 0 {
                // SAFETY: `pa` is a page-aligned frame inside a region Limine
                // marked usable, so it is real RAM, mapped by the HHDM, and not
                // in use by anything else. Each address is visited once, so it
                // cannot already be on the list.
                unsafe { allocator.push(pa) };
                allocator.total += 1;
            }
            pa += FRAME_SIZE;
        }
    }
}

/// Take one frame.
///
/// The lock is held with interrupts off, here and below. Mapping a surface
/// into a task allocates page tables from inside a system call, which runs
/// with interrupts disabled; it must never find this lock held by a task it
/// preempted on the same CPU.
pub fn alloc() -> Option<Frame> {
    without_interrupts(|| ALLOCATOR.lock().alloc())
}

/// # Safety
/// See [`FrameAllocator::free`].
pub unsafe fn free(frame: Frame) {
    // SAFETY: forwarding the caller's obligation unchanged.
    without_interrupts(|| unsafe { ALLOCATOR.lock().free(frame) });
}

/// `(free, total)` frame counts.
pub fn stats() -> (usize, usize) {
    without_interrupts(|| {
        let allocator = ALLOCATOR.lock();
        (allocator.free_frames(), allocator.total_frames())
    })
}
