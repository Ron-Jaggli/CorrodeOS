//! Kernel heap.
//!
//! A first-fit free list, kept sorted by address and **coalesced on free**.
//! The sorting is what makes coalescing possible: adjacent free blocks can only
//! be recognised as adjacent if neighbours are neighbours in the list too.
//!
//! The usual toy version of this skips coalescing, which works fine in a demo
//! and then fragments without bound in anything long-running -- free a thousand
//! 16-byte blocks and you can no longer satisfy a 32-byte request despite
//! having the space. Merging on every insert costs one extra comparison and
//! removes that whole failure mode, so it goes in now rather than after it
//! bites.
//!
//! Allocated blocks carry no header: `dealloc` gets the `Layout` back from the
//! caller, so the size can be recomputed rather than stored. That keeps the
//! per-allocation overhead at zero bytes.

use core::alloc::{GlobalAlloc, Layout};
use core::mem::{align_of, size_of};
use core::ptr;

use crate::sync::SpinLock;

/// Every block is at least this aligned, and at least this large -- a freed
/// region has to be able to hold a `Node`.
const MIN_ALIGN: usize = 16;

/// A free block. Lives *inside* the memory it describes.
#[repr(C)]
struct Node {
    size: usize,
    next: *mut Node,
}

const MIN_BLOCK: usize = size_of::<Node>();

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

pub struct Heap {
    /// Free blocks, ascending by address.
    head: *mut Node,
    total: usize,
    free: usize,
}

// SAFETY: `Heap` owns the memory its raw pointers refer to, and every access
// goes through a `SpinLock`. The pointers are never handed out, so moving the
// struct between contexts cannot create an alias.
unsafe impl Send for Heap {}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            head: ptr::null_mut(),
            total: 0,
            free: 0,
        }
    }

    /// Give the heap its backing memory.
    ///
    /// # Safety
    /// `[start, start + size)` must be mapped, writable, and owned entirely by
    /// the heap for the rest of the kernel's life. Calling this twice, or with
    /// a region already in use, hands out aliased memory.
    pub unsafe fn init(&mut self, start: usize, size: usize) {
        let aligned = align_up(start, MIN_ALIGN);
        let size = (size - (aligned - start)) & !(MIN_ALIGN - 1);
        if size < MIN_BLOCK {
            return;
        }

        let node = aligned as *mut Node;
        // SAFETY: the caller guarantees the region is mapped and writable, and
        // `aligned` is `MIN_ALIGN`-aligned, which satisfies `Node`'s alignment.
        unsafe {
            node.write(Node {
                size,
                next: ptr::null_mut(),
            });
        }

        self.head = node;
        self.total = size;
        self.free = size;
    }

    /// What a request actually costs: big enough to hold a `Node` once freed,
    /// and aligned to at least `MIN_ALIGN`.
    fn size_align(layout: Layout) -> (usize, usize) {
        let align = layout.align().max(MIN_ALIGN);
        let size = align_up(layout.size().max(MIN_BLOCK), MIN_ALIGN);
        (size, align)
    }

    /// Insert a free region, merging with either neighbour if they touch.
    ///
    /// # Safety
    /// `[addr, addr + size)` must be heap memory that is not currently
    /// allocated and not already on the free list.
    unsafe fn insert_free(&mut self, addr: usize, size: usize) {
        let new = addr as *mut Node;

        // Walk to the first block that starts after `addr`.
        let mut prev: *mut Node = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < addr {
            prev = cur;
            // SAFETY: `cur` is a live node on our own free list.
            cur = unsafe { (*cur).next };
        }

        // SAFETY: `addr` is heap memory the caller has handed back, so writing
        // a header there is sound, and the alignment invariant is maintained by
        // every path that produces a free region.
        unsafe {
            new.write(Node { size, next: cur });
        }

        if prev.is_null() {
            self.head = new;
        } else {
            // SAFETY: `prev` is a live node on the free list.
            unsafe { (*prev).next = new };
        }

        // Merge forward: does this block end exactly where the next begins?
        if !cur.is_null() && addr + size == cur as usize {
            // SAFETY: both `new` and `cur` are live nodes we just linked.
            unsafe {
                (*new).size += (*cur).size;
                (*new).next = (*cur).next;
            }
        }

        // Merge backward: does the previous block end exactly where this
        // begins? Done second so a three-way merge collapses in one pass.
        if !prev.is_null() {
            // SAFETY: `prev` is a live node on the free list.
            unsafe {
                if prev as usize + (*prev).size == addr {
                    (*prev).size += (*new).size;
                    (*prev).next = (*new).next;
                }
            }
        }
    }

    /// # Safety
    /// Standard `GlobalAlloc::alloc` contract.
    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let (size, align) = Self::size_align(layout);

        let mut prev: *mut Node = ptr::null_mut();
        let mut cur = self.head;

        while !cur.is_null() {
            // SAFETY: `cur` is a live node on our free list.
            let (block_size, next) = unsafe { ((*cur).size, (*cur).next) };
            let block_start = cur as usize;
            let alloc_start = align_up(block_start, align);
            let front = alloc_start - block_start;

            // Does the request fit, and are both leftovers either zero or big
            // enough to be a block in their own right? A 4-byte scrap can't
            // hold a Node, so it can never be tracked or reused.
            let fits = alloc_start
                .checked_add(size)
                .is_some_and(|end| end <= block_start + block_size);

            if fits {
                let end = alloc_start + size;
                let back = block_start + block_size - end;

                if (front == 0 || front >= MIN_BLOCK) && (back == 0 || back >= MIN_BLOCK) {
                    // Unlink the whole block, then give back the leftovers.
                    if prev.is_null() {
                        self.head = next;
                    } else {
                        // SAFETY: `prev` is a live node.
                        unsafe { (*prev).next = next };
                    }

                    if front != 0 {
                        // SAFETY: the front scrap is heap memory we just
                        // unlinked and is not allocated.
                        unsafe { self.insert_free(block_start, front) };
                    }
                    if back != 0 {
                        // SAFETY: as above for the tail scrap.
                        unsafe { self.insert_free(end, back) };
                    }

                    self.free -= size;
                    return alloc_start as *mut u8;
                }
            }

            prev = cur;
            cur = next;
        }

        ptr::null_mut()
    }

    /// # Safety
    /// Standard `GlobalAlloc::dealloc` contract: `ptr` came from `alloc` with
    /// this exact `layout`.
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Self::size_align(layout);
        // SAFETY: caller guarantees this region was allocated and is now dead.
        unsafe { self.insert_free(ptr as usize, size) };
        self.free += size;
    }

    /// Number of blocks on the free list. One, after everything is freed, means
    /// coalescing is working.
    fn block_count(&self) -> usize {
        let mut count = 0;
        let mut cur = self.head;
        while !cur.is_null() {
            count += 1;
            // SAFETY: `cur` is a live node on our free list.
            cur = unsafe { (*cur).next };
        }
        count
    }
}

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
    (heap.free, heap.total, heap.block_count())
}

const _: () = assert!(align_of::<Node>() <= MIN_ALIGN);
