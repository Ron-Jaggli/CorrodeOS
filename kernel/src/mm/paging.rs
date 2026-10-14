//! Four-level page tables.
//!
//! Until now we have been running on Limine's page tables. They work, but they
//! belong to the bootloader, its memory is marked reclaimable, and its mappings
//! are deliberately permissive -- the kernel's own `.text` is writable. Both of
//! those are things we want to stop being true.
//!
//! What we build here:
//!
//! * The higher-half direct map, at the same offset Limine used so every
//!   pointer we already hold stays valid. 2 MiB pages, writable, never
//!   executable.
//! * The kernel image at -2 GiB, 4 KiB pages, one permission set per section:
//!   `.text` is read+execute, `.rodata` is read-only+NX, `.data`/`.bss` are
//!   read+write+NX. That is W^X, and it is only meaningful because the linker
//!   script pads each section to a page boundary.
//!
//! Everything is reached through the HHDM, so building tables never needs a
//! temporary mapping or a recursive slot.

use crate::mm::frame;
use crate::sync::SpinLock;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
/// Reachable from ring 3.
///
/// The CPU ANDs this bit down the whole walk, so a leaf marked USER under an
/// interior entry that is not stays unreachable. `descend` therefore propagates
/// it upward, which is safe in the other direction too: a kernel leaf under a
/// USER interior entry is still kernel-only, because both have to agree.
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;

/// Bits 12..51 of an entry: the physical address of the next table or page.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;
const PAGE_1G: u64 = 1024 * 1024 * 1024;

/// Index into the table at `level` (4 = PML4 .. 1 = PT) for a virtual address.
const fn index(va: u64, level: u32) -> usize {
    let shift = 12 + 9 * (level - 1);
    ((va >> shift) & 0x1ff) as usize
}

/// An address space: a PML4 and the offset that reaches physical memory.
///
/// `Copy` because it is two numbers and owns nothing. Dropping one frees no
/// frames -- tearing an address space down means walking it, and nothing here
/// destroys one yet.
#[derive(Clone, Copy)]
pub struct Mapper {
    /// Physical address of the PML4.
    root: u64,
    hhdm: u64,
}

impl Mapper {
    /// Allocate an empty top-level table.
    pub fn new(hhdm: u64) -> Option<Self> {
        let frame = frame::alloc()?;
        let mapper = Self {
            root: frame.addr(),
            hhdm,
        };
        // SAFETY: the frame came from the allocator, so it is real RAM mapped
        // by the HHDM and owned exclusively by us.
        unsafe { mapper.zero(frame.addr()) };
        Some(mapper)
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    /// # Safety
    /// `pa` must be a page-aligned frame of real RAM covered by the HHDM and
    /// not currently in use by anything else.
    unsafe fn zero(&self, pa: u64) {
        let ptr = (pa + self.hhdm) as *mut u64;
        for i in 0..512 {
            // SAFETY: caller guarantees a full 4 KiB frame at `pa`, so all 512
            // u64 slots are in bounds and writable.
            unsafe { ptr.add(i).write(0) };
        }
    }

    /// # Safety
    /// `pa` must be the physical address of a live page table.
    unsafe fn entry(&self, pa: u64, index: usize) -> *mut u64 {
        // SAFETY: caller guarantees `pa` is a page table, and `index` is masked
        // to 0..512 by its callers, so this stays inside the frame.
        unsafe { ((pa + self.hhdm) as *mut u64).add(index) }
    }

    /// Follow one level down, allocating the next table if it is missing.
    ///
    /// # Safety
    /// `table` must be the physical address of a live page table at the level
    /// above the one being returned.
    unsafe fn descend(&self, table: u64, index: usize, leaf_flags: u64) -> Option<u64> {
        // SAFETY: caller guarantees `table` is a page table.
        let slot = unsafe { self.entry(table, index) };
        // SAFETY: as above; the slot is inside that table.
        let value = unsafe { slot.read() };

        if value & PRESENT != 0 {
            // A huge page here means someone already mapped this range at a
            // coarser granularity; splitting it is not something we do.
            if value & HUGE != 0 {
                return None;
            }
            // An interior entry on the path to a ring 3 page has to allow ring
            // 3, or the AND down the walk denies the leaf. Widening here cannot
            // expose a kernel leaf underneath: that leaf still has to carry
            // USER itself, and it does not.
            if leaf_flags & USER != 0 && value & USER == 0 {
                // SAFETY: `slot` is a valid entry in a live table.
                unsafe { slot.write(value | USER) };
            }
            return Some(value & ADDR_MASK);
        }

        let next = frame::alloc()?;
        // SAFETY: fresh frame from the allocator.
        unsafe { self.zero(next.addr()) };

        // Intermediate entries stay permissive: the CPU ANDs permissions down
        // the walk, so a restriction here would silently override the leaf.
        // In particular NX must NOT be set on an interior entry, or everything
        // below it becomes non-executable.
        // SAFETY: `slot` is a valid entry in a live table.
        unsafe { slot.write(next.addr() | PRESENT | WRITABLE | (leaf_flags & USER)) };
        Some(next.addr())
    }

    /// Build a new address space that shares this one's kernel half.
    ///
    /// The upper 256 PML4 entries are copied by value, so both spaces point at
    /// the *same* lower-level kernel tables. That is what makes a trap taken in
    /// ring 3 land on a mapped kernel stack whichever address space was active,
    /// and it means a kernel mapping added later -- another task stack, another
    /// heap page -- appears in every space at once, because the table it is
    /// written into is shared.
    ///
    /// The exception is a kernel mapping that needs a *new* PML4 entry. That
    /// writes into this space's top-level table only, and spaces forked earlier
    /// will not see it. Nothing does that today: the direct map, heap, stack
    /// area and kernel image each occupy one slot, established at boot before
    /// any user space exists. It would stop being true if the stack area ever
    /// grew past its 512 GiB slot.
    ///
    /// The lower half is left empty, which is the whole point: user mappings go
    /// there, and one space's are invisible to another's.
    pub fn fork_kernel_half(&self) -> Option<Mapper> {
        let frame = frame::alloc()?;
        let forked = Mapper {
            root: frame.addr(),
            hhdm: self.hhdm,
        };

        // SAFETY: the frame came from the allocator, so it is real RAM covered
        // by the HHDM and owned exclusively by us.
        unsafe { forked.zero(frame.addr()) };

        for slot in 256..512 {
            // SAFETY: both roots are live page tables and `slot` is in range.
            unsafe {
                let entry = self.entry(self.root, slot).read();
                self.entry(forked.root, slot).write(entry);
            }
        }

        Some(forked)
    }

    /// Physical address a virtual address resolves to in *this* space.
    ///
    /// Walks the tables rather than consulting the TLB or the active CR3, so it
    /// answers for an address space that is not currently installed -- which is
    /// the only way one address space can be compared against another.
    pub fn translate(&self, va: u64) -> Option<u64> {
        // SAFETY: `self.root` is a live PML4 and every table reached from it by
        // a PRESENT entry is a live table of the next level down.
        unsafe {
            let p4 = self.entry(self.root, index(va, 4)).read();
            if p4 & PRESENT == 0 {
                return None;
            }

            let p3 = self.entry(p4 & ADDR_MASK, index(va, 3)).read();
            if p3 & PRESENT == 0 {
                return None;
            }
            if p3 & HUGE != 0 {
                return Some((p3 & ADDR_MASK) + (va & (PAGE_1G - 1)));
            }

            let p2 = self.entry(p3 & ADDR_MASK, index(va, 2)).read();
            if p2 & PRESENT == 0 {
                return None;
            }
            if p2 & HUGE != 0 {
                return Some((p2 & ADDR_MASK) + (va & (PAGE_2M - 1)));
            }

            let p1 = self.entry(p2 & ADDR_MASK, index(va, 1)).read();
            if p1 & PRESENT == 0 {
                return None;
            }
            Some((p1 & ADDR_MASK) + (va & (PAGE_4K - 1)))
        }
    }

    /// The permission bits of the 4 KiB page mapping `va` in this space, if
    /// there is one: everything in the leaf entry that is not the address.
    ///
    /// For checks that ask what a mapping *allows* rather than where it
    /// goes. Only the leaf is reported; the walk above it is as permissive as
    /// `descend` made it.
    pub fn leaf_flags(&self, va: u64) -> Option<u64> {
        // SAFETY: as `translate`.
        unsafe {
            let p4 = self.entry(self.root, index(va, 4)).read();
            if p4 & PRESENT == 0 {
                return None;
            }
            let p3 = self.entry(p4 & ADDR_MASK, index(va, 3)).read();
            if p3 & PRESENT == 0 || p3 & HUGE != 0 {
                return None;
            }
            let p2 = self.entry(p3 & ADDR_MASK, index(va, 2)).read();
            if p2 & PRESENT == 0 || p2 & HUGE != 0 {
                return None;
            }
            let p1 = self.entry(p2 & ADDR_MASK, index(va, 1)).read();
            if p1 & PRESENT == 0 {
                return None;
            }
            Some(p1 & !ADDR_MASK)
        }
    }

    /// Wipe a freshly allocated frame before anything else can see it.
    ///
    /// # Safety
    /// `pa` must be a page-aligned frame this caller exclusively owns.
    pub unsafe fn zero_frame(&self, pa: u64) {
        // SAFETY: forwarded from the caller's guarantee.
        unsafe { self.zero(pa) }
    }

    /// Copy bytes into a frame through the direct map.
    ///
    /// # Safety
    /// `pa` must be a page-aligned frame this caller exclusively owns, and
    /// `offset + bytes.len()` must be at most 4096.
    pub unsafe fn write_frame(&self, pa: u64, offset: usize, bytes: &[u8]) {
        // SAFETY: the caller owns the frame and promises the range is inside
        // it; `bytes` is a live slice elsewhere, so the two do not overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                ((pa + self.hhdm) as *mut u8).add(offset),
                bytes.len(),
            );
        }
    }

    /// Map one 4 KiB page. `flags` should not include the address or PRESENT.
    pub fn map_4k(&mut self, va: u64, pa: u64, flags: u64) -> bool {
        // SAFETY: `self.root` is the PML4 we allocated and every `descend`
        // returns a live table of the next level down.
        unsafe {
            let Some(p3) = self.descend(self.root, index(va, 4), flags) else {
                return false;
            };
            let Some(p2) = self.descend(p3, index(va, 3), flags) else {
                return false;
            };
            let Some(p1) = self.descend(p2, index(va, 2), flags) else {
                return false;
            };
            self.entry(p1, index(va, 1))
                .write((pa & ADDR_MASK) | flags | PRESENT);
        }
        true
    }

    /// Map one 2 MiB page. `va` and `pa` must both be 2 MiB aligned.
    pub fn map_2m(&mut self, va: u64, pa: u64, flags: u64) -> bool {
        // SAFETY: as `map_4k`, stopping one level higher and setting HUGE so
        // the CPU treats the entry as a leaf rather than a table pointer.
        unsafe {
            let Some(p3) = self.descend(self.root, index(va, 4), flags) else {
                return false;
            };
            let Some(p2) = self.descend(p3, index(va, 3), flags) else {
                return false;
            };
            self.entry(p2, index(va, 2))
                .write((pa & ADDR_MASK) | flags | HUGE | PRESENT);
        }
        true
    }

    /// Direct-map one physical region, rounded out to 2 MiB boundaries.
    ///
    /// Deliberately per-region rather than `[0, max_phys)`: the firmware memory
    /// map can contain a reserved entry near the top of the address space, and
    /// spanning the gap to reach it would cost a page table per gigabyte of
    /// nothing. Mapping only the regions that exist keeps this at a handful of
    /// frames.
    ///
    /// Overlap between regions that share a 2 MiB page is fine -- the physical
    /// address is a fixed function of the virtual one, so a repeated mapping
    /// writes an identical entry.
    ///
    /// Returns the number of 2 MiB pages installed.
    pub fn map_hhdm_region(&mut self, base: u64, len: u64) -> u64 {
        if len == 0 {
            return 0;
        }
        let start = base / PAGE_2M * PAGE_2M;
        let end = (base + len).div_ceil(PAGE_2M) * PAGE_2M;

        let mut pa = start;
        let mut pages = 0u64;
        while pa < end {
            // GLOBAL: the direct map is identical in every address space, so
            // its TLB entries survive a CR3 reload once we have userspace.
            if !self.map_2m(pa + self.hhdm, pa, WRITABLE | GLOBAL | NO_EXECUTE) {
                break;
            }
            pa += PAGE_2M;
            pages += 1;
        }
        pages
    }

    /// Map a device register page.
    ///
    /// MMIO must never be cached: the CPU would happily serve a stale value for
    /// a status register, or merge two writes into one and drop a command.
    /// PCD+PWT forces every access out to the device.
    pub fn map_mmio(&mut self, va: u64, pa: u64) -> bool {
        self.map_4k(
            va,
            pa,
            WRITABLE | NO_EXECUTE | GLOBAL | NO_CACHE | WRITE_THROUGH,
        )
    }

    /// Map a kernel section, virtual `[start, end)`, with `flags`.
    ///
    /// Returns the number of 4 KiB pages installed.
    pub fn map_kernel_range(
        &mut self,
        virt_start: u64,
        virt_end: u64,
        virt_base: u64,
        phys_base: u64,
        flags: u64,
    ) -> u64 {
        let start = virt_start / PAGE_4K * PAGE_4K;
        let end = virt_end.div_ceil(PAGE_4K) * PAGE_4K;

        let mut va = start;
        let mut pages = 0u64;
        while va < end {
            let pa = phys_base + (va - virt_base);
            if !self.map_4k(va, pa, flags | GLOBAL) {
                break;
            }
            va += PAGE_4K;
            pages += 1;
        }
        pages
    }

    /// Switch this CPU to these page tables.
    ///
    /// # Safety
    /// Every address the CPU will touch immediately afterwards must already be
    /// mapped here: the instruction pointer, the stack, the GDT/IDT, and the
    /// page tables themselves. Getting this wrong is a triple fault, which on
    /// real hardware is an instant reboot with no diagnostics.
    pub unsafe fn activate(&self) {
        // SAFETY: caller guarantees the new tables cover everything live. The
        // write to CR3 also flushes all non-global TLB entries.
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) self.root, options(nostack, preserves_flags));
        }
    }
}

/// The kernel's address space, kept for later use.
///
/// Previously this was leaked after `activate`. It has to live somewhere
/// reachable: mapping the heap, and later MMIO for the APIC, both need to add
/// entries long after boot.
static KERNEL_MAPPER: SpinLock<Option<Mapper>> = SpinLock::new(None);

/// Hand the kernel address space over for safekeeping.
/// Where the kernel image sits, as Limine reported it: virtual base and the
/// physical base it was loaded at.
///
/// Recorded because the kernel image is the one thing whose physical address
/// cannot be recovered from a pointer -- the HHDM inverts trivially, the image
/// mapping does not. Ring 3 needs it: a user program's code lives in the kernel
/// image, and running it from ring 3 means mapping those same frames a second
/// time at a user address.
static KERNEL_IMAGE: SpinLock<Option<(u64, u64)>> = SpinLock::new(None);

pub fn set_kernel_image_base(virtual_base: u64, physical_base: u64) {
    *KERNEL_IMAGE.lock() = Some((virtual_base, physical_base));
}

/// Physical address backing a kernel-image virtual address.
///
/// Only valid for addresses inside the image; the HHDM, the heap and task
/// stacks are mapped by different rules and this would give a wrong answer for
/// them, so it returns `None` rather than guessing.
pub fn kernel_phys_of(va: u64) -> Option<u64> {
    let (virtual_base, physical_base) = (*KERNEL_IMAGE.lock())?;
    let offset = va.checked_sub(virtual_base)?;
    Some(physical_base + offset)
}

pub fn set_kernel_mapper(mapper: Mapper) {
    *KERNEL_MAPPER.lock() = Some(mapper);
}

/// Physical address of the kernel address space's PML4.
pub fn kernel_root() -> Option<u64> {
    Some(KERNEL_MAPPER.lock().as_ref()?.root)
}

/// Install an address space by root.
///
/// # Safety
/// `root` must be a live PML4 that maps the kernel half -- the code executing
/// this instruction, the stack it is on, and the tables themselves all have to
/// stay reachable across the write, because the very next instruction fetch
/// goes through the new tables.
pub unsafe fn activate_root(root: u64) {
    // SAFETY: forwarded from the caller. The write to CR3 also flushes every
    // non-global TLB entry.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// Run `f` against the kernel address space, if it has been installed.
///
/// Note that adding a mapping does not flush the TLB; for a previously
/// unmapped address that is fine, because there is no stale entry to evict.
pub fn with_kernel_mapper<R>(f: impl FnOnce(&mut Mapper) -> R) -> Option<R> {
    KERNEL_MAPPER.lock().as_mut().map(f)
}
