//! Kernel stacks for tasks.
//!
//! Each stack gets its own virtual range with an **unmapped guard page below
//! it**. That is the whole reason these are not just heap allocations: a task
//! that overruns its stack hits the guard page and takes a clean page fault at
//! the exact instruction responsible, instead of quietly scribbling over
//! whatever the allocator happened to place underneath.
//!
//! Ranges are handed out by bumping a cursor, and a finished task's stack
//! comes back through [`release`] to be handed out again whole, still mapped.
//! Never unmapped: the frames stay the stack's, so no other CPU's TLB can
//! hold a stale entry for memory that has become something else. When a
//! stack is out of use is the scheduler's to know (`task::reap`).

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::vec::Vec;

use crate::arch::x86_64::without_interrupts;
use crate::mm::frame;
use crate::mm::paging;
use crate::sync::SpinLock;

/// Virtual region set aside for task stacks. PML4 slot 384: clear of the direct
/// map (256), the heap (320), MMIO (352-ish) and the kernel image (511).
const STACK_AREA_BASE: u64 = 0xffff_c000_0000_0000;

const PAGE: u64 = 4096;

/// 32 KiB of usable stack per task. 16 KiB was enough until a kernel
/// service (the window server, for the terminal's `win`) loaded a Windows
/// program: the PE reader keeps its import table, 64 entries, on the stack.
const STACK_PAGES: u64 = 8;
/// One unmapped page below each stack.
const GUARD_PAGES: u64 = 1;

const SLOT_SIZE: u64 = (STACK_PAGES + GUARD_PAGES) * PAGE;

static NEXT_SLOT: AtomicU64 = AtomicU64::new(STACK_AREA_BASE);

/// Stacks given back, mapped and ready.
static FREE: SpinLock<Vec<Stack>> = SpinLock::new(Vec::new());

#[derive(Debug, Clone, Copy)]
pub struct Stack {
    /// Lowest mapped address.
    pub bottom: u64,
    /// One past the highest mapped address; the stack pointer starts here and
    /// grows down.
    pub top: u64,
    /// The unmapped page immediately below `bottom`.
    pub guard: u64,
}

/// A stack: one given back, or a fresh range carved out and mapped.
pub fn allocate() -> Option<Stack> {
    if let Some(stack) = without_interrupts(|| FREE.lock().pop()) {
        return Some(stack);
    }
    let slot = NEXT_SLOT.fetch_add(SLOT_SIZE, Ordering::Relaxed);

    // Guard first, then the stack above it -- stacks grow down, so the guard
    // has to sit at the low end to catch an overrun.
    let guard = slot;
    let bottom = slot + GUARD_PAGES * PAGE;
    let top = bottom + STACK_PAGES * PAGE;

    for i in 0..STACK_PAGES {
        let frame = frame::alloc()?;
        let va = bottom + i * PAGE;
        let mapped = paging::with_kernel_mapper(|mapper| {
            mapper.map_4k(
                va,
                frame.addr(),
                paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
            )
        });
        if mapped != Some(true) {
            return None;
        }
    }

    Some(Stack { bottom, top, guard })
}

/// Take back a stack from [`allocate`] that nothing runs on, or ever will
/// again, to be handed out whole to a later task.
pub fn release(stack: Stack) {
    without_interrupts(|| FREE.lock().push(stack));
}
