//! Kernel stacks for tasks.
//!
//! Each stack gets its own virtual range with an **unmapped guard page below
//! it**. That is the whole reason these are not just heap allocations: a task
//! that overruns its stack hits the guard page and takes a clean page fault at
//! the exact instruction responsible, instead of quietly scribbling over
//! whatever the allocator happened to place underneath.
//!
//! Stacks are never reclaimed yet. Ranges are handed out by bumping a cursor,
//! so a long-lived system that spawns and exits many tasks would eventually run
//! out of address space. Fixing that needs a free list of ranges and a way to
//! know no CPU is still running on the stack, neither of which matters while
//! tasks are created once at boot.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::frame;
use crate::mm::paging;

/// Virtual region set aside for task stacks. PML4 slot 384: clear of the direct
/// map (256), the heap (320), MMIO (352-ish) and the kernel image (511).
const STACK_AREA_BASE: u64 = 0xffff_c000_0000_0000;

const PAGE: u64 = 4096;

/// 16 KiB of usable stack per task.
const STACK_PAGES: u64 = 4;
/// One unmapped page below each stack.
const GUARD_PAGES: u64 = 1;

const SLOT_SIZE: u64 = (STACK_PAGES + GUARD_PAGES) * PAGE;

static NEXT_SLOT: AtomicU64 = AtomicU64::new(STACK_AREA_BASE);

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

/// Carve out and map one task stack.
pub fn allocate() -> Option<Stack> {
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
