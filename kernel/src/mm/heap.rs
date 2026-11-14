//! Kernel heap: the allocator in `corrode-heap`, behind a lock, over a
//! region mapped at boot. The same allocator is every program's heap; see
//! that crate for how it works and why it coalesces.

use core::alloc::{GlobalAlloc, Layout};

use corrode_heap::Heap;

use crate::sync::SpinLock;

pub struct LockedHeap(SpinLock<Heap>);

impl LockedHeap {
    pub const fn new() -> Self {
        Self(SpinLock::new(Heap::empty()))
    }
}

// SAFETY: every entry point takes the lock, so the underlying `Heap` is never
// touched concurrently. `alloc` returns either null or a pointer to a region
// removed from the free list, which no other allocation can hand out until it
// is returned via `dealloc`.
//
// The lock is only ever held with interrupts off. A system call allocates --
// a reply capability is an `Arc` -- and arrives with interrupts already
// disabled, so if it could find the lock held by a task it had preempted on
// the same CPU, it would spin on it for ever.
unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarding the caller's `GlobalAlloc` obligations.
        crate::arch::x86_64::without_interrupts(|| unsafe { self.0.lock().alloc(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        crate::arch::x86_64::without_interrupts(|| unsafe { self.0.lock().dealloc(ptr, layout) })
    }
}

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::new();

/// Where the kernel heap lives. PML4 slot 320 -- clear of the direct map at
/// slot 256 and the kernel image at slot 511.
pub const HEAP_START: usize = 0xffff_a000_0000_0000;
pub const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Hand the mapped heap region to the allocator.
///
/// # Safety
/// `[start, start + size)` must already be mapped writable and reserved for the
/// heap alone.
pub unsafe fn init(start: usize, size: usize) {
    // SAFETY: forwarding the caller's obligation.
    unsafe { ALLOCATOR.0.lock().init(start, size) };
}

/// `(free bytes, total bytes, free block count)`.
pub fn stats() -> (usize, usize, usize) {
    let heap = ALLOCATOR.0.lock();
    (heap.free(), heap.total(), heap.block_count())
}
