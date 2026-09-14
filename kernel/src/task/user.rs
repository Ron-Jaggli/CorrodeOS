//! The user half of the address space.
//!
//! A ring 3 task needs two things the kernel's own tasks do not: code it is
//! allowed to execute, and a stack it is allowed to write. Both live in the
//! lower half of the address space, and both carry the USER bit. Nothing the
//! kernel uses does, so ring 3 protection here is real rather than nominal --
//! a user task that dereferences a kernel address takes a page fault, which
//! `./corrode user-test` demonstrates on purpose.
//!
//! # One address space, for now
//!
//! There is still a single CR3. Kernel memory is unreachable from ring 3
//! because the kernel's mappings lack the USER bit, so the ring boundary holds
//! -- but user tasks share the lower half with each other, so one could read
//! another's stack. Per-task address spaces are what fixes that, and they want
//! a capability for the address space itself rather than a flag on a task.
//! Recorded as a real gap rather than implied to be finished.
//!
//! # Where the code comes from
//!
//! There is no loader and no filesystem, so a user program is a function in the
//! kernel image, whose frames are mapped a second time at a user address. It
//! follows that such a function must be self-contained: a call to another
//! kernel function, or a string literal in `.rodata`, would be a reference to
//! an address ring 3 cannot reach. That is why user programs here are written
//! as naked assembly rather than ordinary Rust -- not for speed, but because
//! the compiler cannot be told that this one function may not touch anything
//! outside its own page.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::frame;
use crate::mm::paging;

const PAGE: u64 = 4096;

/// Lower-half base for aliased user code. Well clear of the null page, so a
/// jump through a zeroed pointer still faults.
const CODE_AREA_BASE: u64 = 0x0000_0000_0040_0000;
/// Pages mapped per program. Two, so a routine that straddles a page boundary
/// is still fully mapped; there is no loader to tell us the real length.
const CODE_PAGES: u64 = 2;

/// Lower-half base for user stacks, far enough above the code area that an
/// overrun cannot reach it.
const STACK_AREA_BASE: u64 = 0x0000_0000_1000_0000;
const STACK_PAGES: u64 = 4;
const GUARD_PAGES: u64 = 1;
const STACK_SLOT: u64 = (STACK_PAGES + GUARD_PAGES) * PAGE;

static NEXT_CODE: AtomicU64 = AtomicU64::new(CODE_AREA_BASE);
static NEXT_STACK: AtomicU64 = AtomicU64::new(STACK_AREA_BASE);

/// Where a user program's code and stack ended up.
#[derive(Debug, Clone, Copy)]
pub struct Image {
    /// Ring 3 address of the entry point.
    pub entry: u64,
    /// Initial user stack pointer.
    pub stack_top: u64,
    /// The unmapped page below the user stack.
    pub stack_guard: u64,
}

/// Map a kernel-image function and a fresh stack into ring 3.
///
/// `entry` is the kernel virtual address of the program's first instruction.
pub fn create(entry: u64) -> Option<Image> {
    let user_entry = map_code(entry)?;
    let (stack_top, stack_guard) = map_stack()?;

    // A user entry has nowhere to return to -- there is no ring 3 runtime
    // beneath it -- so leave a null return address. Returning then faults at 0
    // with the culprit still on the stack, rather than jumping somewhere that
    // happens to be mapped.
    let rsp = stack_top - 8;
    // SAFETY: `stack_top - 8` is inside the range `map_stack` just mapped
    // writable, in the address space that is currently active, and nothing else
    // refers to this stack yet.
    unsafe {
        (rsp as *mut u64).write(0);
    }

    Some(Image {
        entry: user_entry,
        stack_top: rsp,
        stack_guard,
    })
}

/// Alias the frames holding `entry` at a user address, read + execute.
///
/// Not writable, so W^X holds on this side of the ring boundary too: a user
/// program cannot rewrite its own code, and the kernel's copy of those same
/// frames stays read-only through its own mapping.
fn map_code(entry: u64) -> Option<u64> {
    let page = entry & !(PAGE - 1);
    let offset = entry & (PAGE - 1);
    let phys = paging::kernel_phys_of(page)?;

    let base = NEXT_CODE.fetch_add(CODE_PAGES * PAGE, Ordering::Relaxed);
    for i in 0..CODE_PAGES {
        let mapped = paging::with_kernel_mapper(|mapper| {
            mapper.map_4k(base + i * PAGE, phys + i * PAGE, paging::USER)
        });
        if mapped != Some(true) {
            return None;
        }
    }

    Some(base + offset)
}

/// Carve out one user stack with a guard page below it.
fn map_stack() -> Option<(u64, u64)> {
    let slot = NEXT_STACK.fetch_add(STACK_SLOT, Ordering::Relaxed);
    let guard = slot;
    let bottom = slot + GUARD_PAGES * PAGE;
    let top = bottom + STACK_PAGES * PAGE;

    for i in 0..STACK_PAGES {
        let frame = frame::alloc()?;
        let mapped = paging::with_kernel_mapper(|mapper| {
            mapper.map_4k(
                bottom + i * PAGE,
                frame.addr(),
                paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
            )
        });
        if mapped != Some(true) {
            return None;
        }
    }

    Some((top, guard))
}
