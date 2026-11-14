//! A heap, for programs that ask for one.
//!
//! Nothing is allocated until a program says where its memory comes from:
//! a pool capability, from which [`init`] buys pages and maps them. Until
//! then every allocation fails, which for `alloc`'s types means a panic --
//! a program that never set a heap up and uses one finds out at once.
//!
//! The allocator is `corrode-heap`, the same one the kernel uses.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, Ordering};

use corrode_heap::Heap;

use crate::{abi, check, sys, Result};

/// A heap behind a flag. A program is one task on one CPU, so the flag is
/// never contended; it is there so that sharing the heap is sound if that
/// ever stops being true.
struct Locked {
    busy: AtomicBool,
    heap: core::cell::UnsafeCell<Heap>,
}

// SAFETY: every use of `heap` is between taking and releasing `busy`.
unsafe impl Sync for Locked {}

impl Locked {
    fn with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        while self
            .busy
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: `busy` is ours until it is released below.
        let result = f(unsafe { &mut *self.heap.get() });
        self.busy.store(false, Ordering::Release);
        result
    }
}

static HEAP: Locked = Locked {
    busy: AtomicBool::new(false),
    heap: core::cell::UnsafeCell::new(Heap::empty()),
};

struct Global;

// SAFETY: forwards to `Heap`, which hands out disjoint blocks of memory it
// was given and takes them back by the same layout.
unsafe impl GlobalAlloc for Global {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.with(|heap| unsafe { heap.alloc(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP.with(|heap| unsafe { heap.dealloc(ptr, layout) })
    }
}

#[global_allocator]
static GLOBAL: Global = Global;

/// Buy `pages` of memory from the pool in slot `pool`, put the capability to
/// it in slot `into`, and make it the heap -- or more of it, if there is one
/// already. Returns the bytes added.
pub fn init(pool: usize, into: usize, pages: usize) -> Result<usize> {
    let out = sys::call(
        abi::SYS_ALLOC,
        sys::Args {
            rdi: pool as u64,
            rsi: pages as u64,
            rdx: into as u64,
            ..sys::Args::NONE
        },
    );
    check(out.rax)?;
    let mapping = crate::map(into)?;
    // SAFETY: the kernel just made this memory, zeroed, for this task alone,
    // and mapped it writable at `mapping.address` for `mapping.len` bytes.
    // Nothing else in this program knows the address.
    HEAP.with(|heap| unsafe { heap.add_region(mapping.address as usize, mapping.len) });
    Ok(mapping.len)
}

/// Bytes free and bytes in all, for a program that wants to say.
pub fn stats() -> (usize, usize) {
    HEAP.with(|heap| (heap.free(), heap.total()))
}
