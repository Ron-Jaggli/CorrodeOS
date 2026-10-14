//! A first-fit allocator over memory it is handed, coalescing on free.
//!
//! The kernel's heap and every program's heap are this same code. It owns
//! nothing and maps nothing: whoever has memory gives it a region, and it
//! hands out blocks of it. The tests on the build host run it against a model
//! of which bytes are in use.
//!
//! A first-fit free list, kept sorted by address and **coalesced on free**.
//! The sorting is what makes coalescing possible: adjacent free blocks can only
//! be recognised as adjacent if neighbours are neighbours in the list too.
//!
//! The usual toy version of this skips coalescing, which works fine in a demo
//! and then fragments without bound in anything long-running -- free a thousand
//! 16-byte blocks and you can no longer satisfy a 32-byte request despite
//! having the space. Merging on every insert costs one extra comparison and
//! removes that whole failure mode.
//!
//! Allocated blocks carry no header: `dealloc` gets the `Layout` back from the
//! caller, so the size can be recomputed rather than stored. That keeps the
//! per-allocation overhead at zero bytes.

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
mod tests;

use core::alloc::Layout;
use core::mem::size_of;
use core::ptr;

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

// SAFETY: `Heap` owns the memory its raw pointers refer to, and whoever
// shares one between contexts puts it behind a lock. The pointers are never
// handed out, so moving the struct cannot create an alias.
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
        *self = Self::empty();
        // SAFETY: forwarded from the caller.
        unsafe { self.add_region(start, size) };
    }

    /// Give the heap more memory, beside whatever it has already.
    ///
    /// # Safety
    /// As for [`Heap::init`]: `[start, start + size)` must be mapped,
    /// writable, owned by the heap from now on, and not overlap anything it
    /// already has.
    pub unsafe fn add_region(&mut self, start: usize, size: usize) {
        let Some(aligned) = start
            .checked_add(MIN_ALIGN - 1)
            .map(|end| end & !(MIN_ALIGN - 1))
        else {
            return;
        };
        let size = size.saturating_sub(aligned - start) & !(MIN_ALIGN - 1);
        if size < MIN_BLOCK {
            return;
        }
        // SAFETY: the caller hands over a region that is mapped, writable and
        // nobody else's, and `aligned` satisfies `Node`'s alignment.
        unsafe { self.insert_free(aligned, size) };
        self.total += size;
        self.free += size;
    }

    /// Bytes not handed out.
    pub fn free(&self) -> usize {
        self.free
    }

    /// Bytes the heap was given.
    pub fn total(&self) -> usize {
        self.total
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

    /// Hand out a block for `layout`, or null if none fits.
    ///
    /// # Safety
    /// Standard `GlobalAlloc::alloc` contract.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
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
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Self::size_align(layout);
        // SAFETY: caller guarantees this region was allocated and is now dead.
        unsafe { self.insert_free(ptr as usize, size) };
        self.free += size;
    }

    /// Number of blocks on the free list. One, after everything is freed, means
    /// coalescing is working.
    pub fn block_count(&self) -> usize {
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

const _: () = assert!(core::mem::align_of::<Node>() <= MIN_ALIGN);
