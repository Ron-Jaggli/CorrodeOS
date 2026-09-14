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
// Bit 2 is USER. Deliberately absent until there is a ring 3 to grant it to --
// an unused permission flag is an invitation to set it by accident.
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const NO_CACHE: u64 = 1 << 4;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;

/// Bits 12..51 of an entry: the physical address of the next table or page.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Index into the table at `level` (4 = PML4 .. 1 = PT) for a virtual address.
const fn index(va: u64, level: u32) -> usize {
    let shift = 12 + 9 * (level - 1);
    ((va >> shift) & 0x1ff) as usize
}

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
    unsafe fn descend(&self, table: u64, index: usize) -> Option<u64> {
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
        unsafe { slot.write(next.addr() | PRESENT | WRITABLE) };
        Some(next.addr())
    }

    /// Map one 4 KiB page. `flags` should not include the address or PRESENT.
    pub fn map_4k(&mut self, va: u64, pa: u64, flags: u64) -> bool {
        // SAFETY: `self.root` is the PML4 we allocated and every `descend`
        // returns a live table of the next level down.
        unsafe {
            let Some(p3) = self.descend(self.root, index(va, 4)) else {
                return false;
            };
            let Some(p2) = self.descend(p3, index(va, 3)) else {
                return false;
            };
            let Some(p1) = self.descend(p2, index(va, 2)) else {
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
            let Some(p3) = self.descend(self.root, index(va, 4)) else {
                return false;
            };
            let Some(p2) = self.descend(p3, index(va, 3)) else {
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
pub fn set_kernel_mapper(mapper: Mapper) {
    *KERNEL_MAPPER.lock() = Some(mapper);
}

/// Run `f` against the kernel address space, if it has been installed.
///
/// Note that adding a mapping does not flush the TLB; for a previously
/// unmapped address that is fine, because there is no stale entry to evict.
pub fn with_kernel_mapper<R>(f: impl FnOnce(&mut Mapper) -> R) -> Option<R> {
    KERNEL_MAPPER.lock().as_mut().map(f)
}
