//! The user half of the address space.
//!
//! A ring 3 task needs two things the kernel's own tasks do not: code it is
//! allowed to execute, and a stack it is allowed to write. Both live in the
//! lower half of the address space, and both carry the USER bit. Nothing the
//! kernel uses does, so ring 3 protection here is real rather than nominal --
//! a user task that dereferences a kernel address takes a page fault, which
//! `./corrode user-test` demonstrates on purpose.
//!
//! # One address space each
//!
//! Every user task gets its own PML4, forked from the kernel's so the upper
//! half -- and only the upper half -- is shared. The lower half starts empty,
//! which is why the addresses below can be *fixed* rather than handed out from
//! a cursor: every user task has its code and its stack at the same virtual
//! addresses, backed by different memory. One task cannot name another's stack,
//! because in its address space that address means its own.
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

use crate::mm::frame;
use crate::mm::paging::{self, Mapper};

const PAGE: u64 = 4096;

/// Where every user program's code goes. Well clear of the null page, so a
/// jump through a zeroed pointer still faults.
pub const CODE_BASE: u64 = 0x0000_0000_0040_0000;
/// Pages mapped per program. Two, so a routine that straddles a page boundary
/// is still fully mapped; there is no loader to tell us the real length.
const CODE_PAGES: u64 = 2;

/// Where every user program's stack goes, far enough above the code that an
/// overrun cannot reach it.
pub const STACK_GUARD: u64 = 0x0000_0000_1000_0000;
const STACK_PAGES: u64 = 4;
const STACK_BOTTOM: u64 = STACK_GUARD + PAGE;
/// One past the highest mapped stack byte, in every user address space.
pub const STACK_TOP: u64 = STACK_BOTTOM + STACK_PAGES * PAGE;

/// Where a user program's code and stack ended up, and the space they are in.
#[derive(Clone, Copy)]
pub struct Image {
    /// Ring 3 address of the entry point.
    pub entry: u64,
    /// Initial user stack pointer.
    pub stack_top: u64,
    /// The unmapped page below the user stack.
    pub stack_guard: u64,
    /// The address space all of the above live in.
    pub space: Mapper,
}

/// Build a fresh address space holding one program and one stack.
///
/// `entry` is the kernel virtual address of the program's first instruction.
pub fn create(entry: u64) -> Option<Image> {
    let mut space = paging::with_kernel_mapper(|kernel| kernel.fork_kernel_half())??;

    let user_entry = map_code(&mut space, entry)?;
    map_stack(&mut space)?;

    Some(Image {
        entry: user_entry,
        // A user entry has nowhere to return to -- there is no ring 3 runtime
        // beneath it -- so it starts on a null return address. Returning faults
        // at 0 with the culprit still on the stack, rather than jumping
        // somewhere that happens to be mapped. The word is already zero because
        // every user frame is wiped before it is mapped.
        stack_top: STACK_TOP - 8,
        stack_guard: STACK_GUARD,
        space,
    })
}

/// Alias the frames holding `entry` at a user address, read + execute.
///
/// Not writable, so W^X holds on this side of the ring boundary too: a user
/// program cannot rewrite its own code, and the kernel's copy of those same
/// frames stays read-only through its own mapping.
///
/// The frames are shared with the kernel's mapping of them rather than copied.
/// That is safe precisely because they are not writable from either side: the
/// text of a program is not private state.
fn map_code(space: &mut Mapper, entry: u64) -> Option<u64> {
    let page = entry & !(PAGE - 1);
    let offset = entry & (PAGE - 1);
    let phys = paging::kernel_phys_of(page)?;

    for i in 0..CODE_PAGES {
        if !space.map_4k(CODE_BASE + i * PAGE, phys + i * PAGE, paging::USER) {
            return None;
        }
    }

    Some(CODE_BASE + offset)
}

/// Map this space's stack, with a guard page below it.
///
/// Every frame is zeroed first. A frame just off the free list holds whatever
/// the last owner left in it -- including the allocator's own list pointers,
/// since the free list is threaded through free frames -- and handing that to
/// ring 3 would be a kernel memory disclosure through a mapping we created
/// deliberately.
fn map_stack(space: &mut Mapper) -> Option<()> {
    for i in 0..STACK_PAGES {
        let frame = frame::alloc()?;

        // SAFETY: the frame came from the allocator, so it is real RAM covered
        // by the HHDM and owned exclusively by us. Zeroing happens through the
        // HHDM, which is why it works before the user mapping exists.
        unsafe { space.zero_frame(frame.addr()) };

        if !space.map_4k(
            STACK_BOTTOM + i * PAGE,
            frame.addr(),
            paging::USER | paging::WRITABLE | paging::NO_EXECUTE,
        ) {
            return None;
        }
    }

    Some(())
}
