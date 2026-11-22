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
//! Two places. A **program** is an ELF file the bootloader left in RAM: its
//! segments are copied into frames of their own and mapped where the file
//! says, each with the permissions the file asks for and never both writable
//! and executable. That is what a driver is. See [`load`].
//!
//! The older way is still here for the first boot checks, which predate the
//! loader: a function in the kernel image, whose frames are mapped a second
//! time at a user address. Such a function must be self-contained -- a call
//! to another kernel function, or a string literal in `.rodata`, would be a
//! reference to an address ring 3 cannot reach -- which is why those are
//! naked assembly. See [`create`].

use corrode_abi::layout;
use corrode_elf::{Elf, Segment};
use corrode_pe::exports::Exports;
use corrode_pe::{Fixup, Pe};

use crate::mm::frame;
use crate::mm::paging::{self, Mapper};

const PAGE: u64 = 4096;

/// Where every user program's code goes. Well clear of the null page, so a
/// jump through a zeroed pointer still faults.
pub const CODE_BASE: u64 = layout::IMAGE_BASE;
/// Pages mapped per program. Two, so a routine that straddles a page boundary
/// is still fully mapped; there is no loader to tell us the real length.
const CODE_PAGES: u64 = 2;

/// Where every user program's stack goes, far enough above the code that an
/// overrun cannot reach it.
pub const STACK_GUARD: u64 = layout::IMAGE_END;
const STACK_PAGES: u64 = 4;
const STACK_BOTTOM: u64 = STACK_GUARD + PAGE;
/// One past the highest mapped stack byte of an assembly program, in every
/// such address space.
pub const STACK_TOP: u64 = STACK_BOTTOM + STACK_PAGES * PAGE;

/// Stack given to a loaded program: 128 KiB. Compiled code keeps more on its
/// stack than a page of hand-written assembly does.
const PROGRAM_STACK_PAGES: u64 = 32;
/// The most stack a Windows program is given: 1 MiB, Windows' own default
/// reserve. It asks with `SizeOfStackReserve` (mingw writes 2 MiB, MSVC
/// 1 MiB); it gets that, between [`PROGRAM_STACK_PAGES`] and this, all of it
/// mapped at once, since nothing here grows a stack on demand.
const MOST_STACK_PAGES: u64 = 256;

/// Pages of stack for a Windows program that asks for `reserve` bytes.
fn windows_stack_pages(reserve: u64) -> u64 {
    reserve.div_ceil(PAGE).clamp(PROGRAM_STACK_PAGES, MOST_STACK_PAGES)
}

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
    /// What the task's GS base is while it runs: a Windows program's TEB,
    /// 0 for everything else.
    pub gs_base: u64,
}

/// Build a fresh address space holding one program and one stack.
///
/// `entry` is the kernel virtual address of the program's first instruction.
pub fn create(entry: u64) -> Option<Image> {
    let mut space = paging::with_kernel_mapper(|kernel| kernel.fork_kernel_half())??;

    let user_entry = map_code(&mut space, entry)?;
    map_stack(&mut space, STACK_PAGES)?;

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
        gs_base: 0,
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
fn map_stack(space: &mut Mapper, pages: u64) -> Option<()> {
    for i in 0..pages {
        let frame = frame::alloc()?;

        // SAFETY: the frame came from the allocator, so it is real RAM covered
        // by the HHDM and owned exclusively by us. Zeroing happens through the
        // HHDM, which is why it works before the user mapping exists.
        unsafe { space.zero_frame(frame.addr()) };

        if !space.map_4k(
            STACK_BOTTOM + i * PAGE,
            frame.addr(),
            paging::USER | paging::WRITABLE | paging::NO_EXECUTE | paging::OWNED,
        ) {
            return None;
        }
    }

    Some(())
}

/// Why a program could not be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file is not a program this kernel will run, and why.
    Format(corrode_elf::Error),
    /// The file is not a Windows program this kernel will run, and why.
    Pe(corrode_pe::Error),
    /// Verdigris's kernel32 has no export table at its base.
    NoExports,
    /// Not enough frames for its memory or the page tables to map it.
    OutOfMemory,
}

/// Build a fresh address space from an ELF file.
///
/// Every check on the file happens in [`Elf::parse`], before a frame is
/// taken: by the time anything is mapped the segments are known to lie
/// inside the user image range, to share no page, and to be never both
/// writable and executable. So this function has one way to fail, running
/// out of memory, and the file cannot make it map anything anywhere it
/// should not.
///
/// The program's memory is **copied** out of the file, not mapped from it.
/// The module stays as the bootloader left it -- the same bytes can be
/// loaded twice, and a program scribbling on its own data scribbles on
/// nobody's file. Read-only segments could alias the module's frames and
/// save the copy; that is an optimisation with a page-alignment condition
/// on the file, and nothing needs it yet.
pub fn load(file: &[u8]) -> Result<Image, LoadError> {
    let elf = Elf::parse(file, layout::IMAGE_BASE..layout::IMAGE_END).map_err(LoadError::Format)?;
    // As for an assembly program: the stack pointer a `call` would have
    // left, over a null return address. A compiled entry point expects
    // exactly that alignment.
    build(elf.entry(), elf.segments(), |_| {}, PROGRAM_STACK_PAGES, 8)
}

/// Build a fresh address space from a Windows program: a PE file, checked
/// whole by [`Pe::parse`] before a frame is taken, exactly as [`load`] does
/// for ELF, and mapped by the same code.
///
/// The one difference is the stack. A Windows function is entered with
/// 32 bytes of "shadow space" above its return address for it to spill its
/// four register arguments into, and with the stack pointer 8 below a
/// 16-byte boundary. The return address is null: a program that returns
/// from its entry point instead of exiting faults, and is stopped.
pub fn load_pe(file: &[u8]) -> Result<Image, LoadError> {
    let pe = Pe::parse(file, layout::IMAGE_BASE..layout::IMAGE_END).map_err(LoadError::Pe)?;
    let stack_pages = windows_stack_pages(pe.stack_reserve());
    build(pe.entry(), pe.segments(), |each| pe.moved_fixups(each), stack_pages, 8 + 32)
}

/// Build a fresh address space from a Windows program that imports from
/// kernel32, and Verdigris's kernel32 (see `verdigris/DESIGN.md`).
///
/// The program must end below `KERNEL32_BASE` and kernel32, an ELF file,
/// lie above it, so the two never share a page. kernel32's export table is
/// at its base; every import the program names is looked up there, must
/// name an address in kernel32's code, and is written into the program's
/// import address table through the kernel's own map before the program
/// runs -- so the table may be in read-only data, as linkers put it, and
/// nothing is ever made writable to fill it in. An import kernel32 does not have refuses the program.
pub fn load_windows(file: &[u8], kernel32: &[u8]) -> Result<Image, LoadError> {
    use corrode_abi::verdigris::{
        KERNEL32_BASE, PEB, START_ENTRY, START_TLS_CALLBACKS, TEB, TEB_PAGES, TLS_BLOCK, TLS_VECTOR,
        WALL_AT, WALL_UNIX,
    };
    let pe = Pe::parse(file, layout::IMAGE_BASE..KERNEL32_BASE).map_err(LoadError::Pe)?;
    let wall = crate::rtc::anchor();
    // kernel32 ends below the TEB, which is the kernel's to lay out.
    let runtime = Elf::parse(kernel32, KERNEL32_BASE..TEB).map_err(LoadError::Format)?;
    let exports = runtime
        .segments()
        .find(|segment| segment.address == KERNEL32_BASE && !segment.writable && !segment.executable)
        .and_then(|segment| Exports::parse(segment.data))
        .ok_or(LoadError::NoExports)?;
    let in_code = |address: &u64| {
        runtime.segments().any(|segment| {
            segment.executable && (segment.address..segment.address + segment.len).contains(address)
        })
    };
    // An import names a function in kernel32's code or, as msvcrt's
    // `_fmode` and `_acmdln` are, a variable in its data: either way an
    // address inside kernel32, never the program's or anyone else's.
    let in_runtime = |address: &u64| {
        runtime
            .segments()
            .any(|segment| (segment.address..segment.address + segment.len).contains(address))
    };
    // The program starts in kernel32 (Windows' loader does the same), which
    // runs its TLS callbacks, then calls its entry and ends it with what
    // the entry returns.
    let start = exports.find(b"verdigris", b"start").filter(in_code).ok_or(LoadError::NoExports)?;
    let mut fixups = alloc::vec::Vec::new();
    pe.bind(|dll, name| exports.find(dll, name).filter(in_runtime), |fixup| fixups.push(fixup))
        .map_err(LoadError::Pe)?;
    if pe.moved() {
        crate::println!(
            "verdigris  : moved from {:#x} to {:#x}, {} relocations",
            pe.preferred_base(),
            pe.base(),
            pe.relocations()
        );
    }
    let tls = pe.tls();
    // The TEB and PEB: zeroed pages of the program's own, with the fields
    // Windows code reads without asking written before the program runs.
    let stack_pages = windows_stack_pages(pe.stack_reserve());
    let stack_base = STACK_BOTTOM + stack_pages * PAGE;
    for (address, value) in [
        (TEB + 0x08, stack_base),   // NT_TIB.StackBase: one past the top
        (TEB + 0x10, STACK_BOTTOM), // NT_TIB.StackLimit
        (TEB + 0x30, TEB),          // NT_TIB.Self
        (TEB + 0x58, if tls.is_some() { TLS_VECTOR } else { 0 }), // ThreadLocalStoragePointer
        (TEB + 0x60, PEB),          // ProcessEnvironmentBlock
        (PEB + 0x10, pe.base()),    // ImageBaseAddress
        (START_ENTRY, pe.entry()),
        (START_TLS_CALLBACKS, tls.map_or(0, |tls| tls.callbacks)),
        (TLS_VECTOR, TLS_BLOCK),
        // The wall clock, as DESIGN.md says a program gets it: at its start.
        (WALL_UNIX, wall.map_or(0, |(unix, _)| unix as u64)),
        (WALL_AT, wall.map_or(0, |(_, at)| at)),
    ] {
        fixups.push(Fixup { address, value });
    }
    let blocks = Segment { address: TEB, len: (TEB_PAGES + 1) * PAGE, data: &[], writable: true, executable: false };
    // The one thread's TLS block, made from the program's template.
    let tls_block = tls.filter(|tls| tls.len > 0).map(|tls| Segment {
        address: TLS_BLOCK,
        len: tls.len,
        data: tls.template,
        writable: true,
        executable: false,
    });
    let mut image = build(
        start,
        pe.segments().chain(runtime.segments()).chain(core::iter::once(blocks)).chain(tls_block),
        // A move's fixups first, then the imports' and the kernel's own,
        // which win where they meet: an import's slot is the import's.
        |each| {
            pe.moved_fixups(&mut *each);
            fixups.iter().for_each(|fixup| each(*fixup));
        },
        stack_pages,
        8 + 32,
    )?;
    image.gs_base = TEB;
    Ok(image)
}

/// An address space being built, given back whole -- every table, and
/// every frame mapped in it -- unless the build finishes. A load that fails
/// part way, out of memory, then leaves nothing behind.
struct Unfinished {
    space: Mapper,
    done: bool,
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if !self.done {
            // SAFETY: the space was made by this load and never installed on
            // any CPU; nothing else has a copy of it, and this one is gone
            // once dropped.
            unsafe { self.space.release_user_half() };
        }
    }
}

/// Map checked segments into a fresh address space with a stack of
/// `stack_pages`, the stack pointer `below` bytes under its top. Then
/// `fixups` gives every fixup (a move's, then the imports', then the
/// kernel's own), each written into the frame behind its address, in the
/// order given: the reader has checked each lies inside a segment and does
/// not straddle two pages. Each is written once, so the load's work grows
/// with the fixups and the pages, never with the two multiplied.
fn build<'a>(
    entry: u64,
    segments: impl Iterator<Item = Segment<'a>>,
    fixups: impl FnOnce(&mut dyn FnMut(Fixup)),
    stack_pages: u64,
    below: u64,
) -> Result<Image, LoadError> {
    let space = paging::with_kernel_mapper(|kernel| kernel.fork_kernel_half())
        .flatten()
        .ok_or(LoadError::OutOfMemory)?;
    let mut unfinished = Unfinished { space, done: false };
    let space = &mut unfinished.space;

    for segment in segments {
        // The segment's frames are copies, this space's alone.
        let mut flags = paging::USER | paging::OWNED;
        if segment.writable {
            flags |= paging::WRITABLE;
        }
        if !segment.executable {
            flags |= paging::NO_EXECUTE;
        }

        for page in segment.pages().step_by(PAGE as usize) {
            let frame = frame::alloc().ok_or(LoadError::OutOfMemory)?;
            // Wiped first: the part of a page the file does not fill is the
            // program's zeroed memory, and must not be whatever the frame's
            // last owner left in it.
            //
            // SAFETY: fresh from the allocator, so ours alone and covered by
            // the direct map.
            unsafe { space.zero_frame(frame.addr()) };

            // The bytes of the file that fall in this page, if any. The
            // segment starts at `address`, which need not be page aligned,
            // and its file bytes may end well before its memory does.
            let data_start = segment.address;
            let data_end = segment.address + segment.data.len() as u64;
            let (from, to) = (page.max(data_start), (page + PAGE).min(data_end));
            if from < to {
                let source =
                    &segment.data[(from - data_start) as usize..(to - data_start) as usize];
                // SAFETY: the frame is ours alone and mapped nowhere else
                // yet. `from - page` plus the length is at most a page,
                // because both ends were clamped to this one.
                unsafe { space.write_frame(frame.addr(), (from - page) as usize, source) };
            }

            if !space.map_4k(page, frame.addr(), flags) {
                // Not mapped, so the space will not give it back: this does.
                //
                // SAFETY: from the allocator just now, and in no table.
                unsafe { frame::free(frame) };
                return Err(LoadError::OutOfMemory);
            }
        }
    }

    // The fixups, through the frames behind the pages. Written with the
    // kernel's direct map, so a page mapped read-only for ring 3 takes them
    // too; nothing runs in this space yet.
    fixups(&mut |fixup: Fixup| {
        let page = fixup.address & !(PAGE - 1);
        let offset = fixup.address - page;
        if offset > PAGE - 8 {
            return;
        }
        if let Some(frame) = space.translate(page) {
            // SAFETY: a frame this load allocated, mapped OWNED at `page`
            // in this space alone; the eight bytes end inside it.
            unsafe { space.write_frame(frame, offset as usize, &fixup.value.to_le_bytes()) };
        }
    });

    map_stack(space, stack_pages).ok_or(LoadError::OutOfMemory)?;

    unfinished.done = true;
    Ok(Image {
        entry,
        stack_top: STACK_BOTTOM + stack_pages * PAGE - below,
        stack_guard: STACK_GUARD,
        space: unfinished.space,
        gs_base: 0,
    })
}
