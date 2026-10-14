//! Just enough ACPI to find the interrupt controllers and the PCI bus.
//!
//! We read the RSDP, follow it to the XSDT (or RSDT on ACPI 1.0), and pull out
//! two tables. The MADT tells us where the local APIC and I/O APICs live, and
//! how the legacy ISA IRQs have been remapped onto global system interrupts.
//! The MCFG tells us where PCI configuration space is mapped.
//!
//! No AML, no interpreter, no ACPICA. Those are needed for power management and
//! device enumeration, not for interrupts, and each one is a large subproject.
//!
//! Every table is read through the HHDM. All reads are unaligned: ACPI packs
//! its structures with no regard for natural alignment, and taking a reference
//! to a misaligned field is undefined behaviour even when the load would have
//! worked.

use alloc::vec::Vec;

/// MADT entry types we care about.
const ENTRY_LOCAL_APIC: u8 = 0;
const ENTRY_IO_APIC: u8 = 1;
const ENTRY_SOURCE_OVERRIDE: u8 = 2;
const ENTRY_LOCAL_APIC_ADDRESS_OVERRIDE: u8 = 5;

const SDT_HEADER_LEN: usize = 36;

#[derive(Debug, Clone, Copy)]
pub struct IoApic {
    // No `id` field: the MADT reports one, but the driver reads it straight
    // from the controller's own ID register, which is authoritative.
    pub address: u64,
    /// First global system interrupt this controller handles.
    pub gsi_base: u32,
}

/// An ISA IRQ that does not land on the global system interrupt of the same
/// number. On most machines IRQ0 is remapped to GSI 2, and routing the timer
/// without honouring that is a classic "no ticks" bug.
#[derive(Debug, Clone, Copy)]
pub struct SourceOverride {
    pub source: u8,
    pub gsi: u32,
    pub flags: u16,
}

impl SourceOverride {
    /// True if the MADT says this line is active low.
    pub fn active_low(&self) -> bool {
        self.flags & 0b11 == 0b11
    }

    /// True if the MADT says this line is level triggered.
    pub fn level_triggered(&self) -> bool {
        (self.flags >> 2) & 0b11 == 0b11
    }
}

pub struct Madt {
    pub lapic_address: u64,
    pub io_apics: Vec<IoApic>,
    pub overrides: Vec<SourceOverride>,
    pub cpu_count: usize,
}

impl Madt {
    /// Translate an ISA IRQ to its global system interrupt, honouring
    /// overrides.
    pub fn gsi_for_irq(&self, irq: u8) -> (u32, bool, bool) {
        for entry in &self.overrides {
            if entry.source == irq {
                return (entry.gsi, entry.active_low(), entry.level_triggered());
            }
        }
        (irq as u32, false, false)
    }

    /// Which I/O APIC owns a given GSI.
    pub fn io_apic_for_gsi(&self, gsi: u32) -> Option<&IoApic> {
        // Without reading each controller's redirection count we can only use
        // the base addresses; picking the highest base at or below the GSI is
        // correct for any sane firmware and exactly right for the single
        // controller QEMU gives us.
        self.io_apics
            .iter()
            .filter(|io| io.gsi_base <= gsi)
            .max_by_key(|io| io.gsi_base)
    }
}

/// # Safety
/// `ptr + offset` must be readable for `size_of::<T>()` bytes.
unsafe fn read_at<T>(ptr: *const u8, offset: usize) -> T {
    // SAFETY: caller guarantees the range is readable. `read_unaligned` imposes
    // no alignment requirement, which is essential for ACPI's packed tables.
    unsafe { ptr.add(offset).cast::<T>().read_unaligned() }
}

/// # Safety
/// `ptr` must be readable for `len` bytes.
unsafe fn checksum_ok(ptr: *const u8, len: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..len {
        // SAFETY: caller guarantees `len` readable bytes from `ptr`.
        sum = sum.wrapping_add(unsafe { *ptr.add(i) });
    }
    sum == 0
}

/// Find a system description table by its four-byte signature.
///
/// Returns where it is, through the HHDM, and its length. The table has
/// passed its own checksum; what is in it is the caller's to interpret.
///
/// `rsdp` is a physical address; `hhdm` is the direct map offset.
///
/// # Safety
/// `rsdp` must point at a genuine ACPI RSDP and the HHDM must cover the
/// firmware's ACPI regions.
pub unsafe fn find_table(rsdp: u64, hhdm: u64, wanted: &[u8; 4]) -> Option<(*const u8, usize)> {
    let base = (rsdp + hhdm) as *const u8;

    // SAFETY: caller guarantees `rsdp` points at an RSDP reachable via HHDM.
    let signature: [u8; 8] = unsafe { read_at(base, 0) };
    if &signature != b"RSD PTR " {
        return None;
    }

    // The first 20 bytes are the ACPI 1.0 RSDP and checksum independently of
    // the 2.0 extension.
    // SAFETY: signature matched, so at least 20 bytes are present.
    if !unsafe { checksum_ok(base, 20) } {
        return None;
    }

    // SAFETY: within the validated RSDP.
    let revision: u8 = unsafe { read_at(base, 15) };

    let (sdt_phys, entry_size) = if revision >= 2 {
        // SAFETY: ACPI 2.0+ RSDP is at least 36 bytes; validate it as a whole.
        let length: u32 = unsafe { read_at(base, 20) };
        if length as usize >= 36 && !unsafe { checksum_ok(base, length as usize) } {
            return None;
        }
        // SAFETY: within the validated RSDP.
        (unsafe { read_at::<u64>(base, 24) }, 8usize)
    } else {
        // SAFETY: within the validated RSDP.
        (unsafe { read_at::<u32>(base, 16) } as u64, 4usize)
    };

    if sdt_phys == 0 {
        return None;
    }

    let sdt = (sdt_phys + hhdm) as *const u8;
    // SAFETY: the RSDP points at a system description table.
    let sdt_len: u32 = unsafe { read_at(sdt, 4) };
    if (sdt_len as usize) < SDT_HEADER_LEN {
        return None;
    }
    // SAFETY: length came from the table's own header.
    if !unsafe { checksum_ok(sdt, sdt_len as usize) } {
        return None;
    }

    let count = (sdt_len as usize - SDT_HEADER_LEN) / entry_size;
    for i in 0..count {
        let offset = SDT_HEADER_LEN + i * entry_size;
        let table_phys = if entry_size == 8 {
            // SAFETY: `offset` is inside the validated table.
            unsafe { read_at::<u64>(sdt, offset) }
        } else {
            // SAFETY: as above.
            unsafe { read_at::<u32>(sdt, offset) as u64 }
        };

        if table_phys == 0 {
            continue;
        }

        let table = (table_phys + hhdm) as *const u8;
        // SAFETY: entries in the RSDT/XSDT point at ACPI tables.
        let signature: [u8; 4] = unsafe { read_at(table, 0) };
        if &signature != wanted {
            continue;
        }
        // SAFETY: every table starts with the common header, length at 4.
        let length = unsafe { read_at::<u32>(table, 4) } as usize;
        // SAFETY: length is from the table's own header.
        if length < SDT_HEADER_LEN || !unsafe { checksum_ok(table, length) } {
            return None;
        }
        return Some((table, length));
    }

    None
}

/// Find and parse the MADT.
///
/// # Safety
/// As [`find_table`].
pub unsafe fn find_madt(rsdp: u64, hhdm: u64) -> Option<Madt> {
    // SAFETY: forwarded from the caller.
    let (table, _) = unsafe { find_table(rsdp, hhdm, b"APIC") }?;
    // SAFETY: signature and checksum matched, so this is a MADT.
    unsafe { parse_madt(table) }
}

/// Find the MCFG, which says where PCI configuration space is in memory.
///
/// Returned as bytes for [`corrode_pci::mcfg`] to read: the layout is that
/// crate's business, and it is tested there.
///
/// # Safety
/// As [`find_table`].
pub unsafe fn find_mcfg(rsdp: u64, hhdm: u64) -> Option<&'static [u8]> {
    // SAFETY: forwarded from the caller.
    let (table, length) = unsafe { find_table(rsdp, hhdm, b"MCFG") }?;
    // SAFETY: `find_table` checksummed `length` bytes at `table`, so they are
    // readable, and firmware tables are never unmapped or rewritten.
    Some(unsafe { core::slice::from_raw_parts(table, length) })
}

/// # Safety
/// `table` must point at a MADT reachable for its full declared length.
unsafe fn parse_madt(table: *const u8) -> Option<Madt> {
    // SAFETY: caller guarantees a MADT.
    let length: u32 = unsafe { read_at(table, 4) };
    if (length as usize) < 44 {
        return None;
    }
    // SAFETY: length is from the table header.
    if !unsafe { checksum_ok(table, length as usize) } {
        return None;
    }

    // SAFETY: offsets 36 and 44 are inside a MADT of at least 44 bytes.
    let mut madt = Madt {
        lapic_address: unsafe { read_at::<u32>(table, 36) } as u64,
        io_apics: Vec::new(),
        overrides: Vec::new(),
        cpu_count: 0,
    };

    let mut offset = 44usize;
    while offset + 2 <= length as usize {
        // SAFETY: `offset + 2 <= length`, so the entry header is in bounds.
        let (kind, entry_len): (u8, u8) =
            unsafe { (read_at(table, offset), read_at(table, offset + 1)) };

        // A zero length would spin here forever on a malformed table.
        if entry_len < 2 || offset + entry_len as usize > length as usize {
            break;
        }

        match kind {
            ENTRY_LOCAL_APIC if entry_len >= 8 => {
                // SAFETY: entry is in bounds and long enough.
                let flags: u32 = unsafe { read_at(table, offset + 4) };
                // Bit 0 enabled, bit 1 online-capable.
                if flags & 0b11 != 0 {
                    madt.cpu_count += 1;
                }
            }
            ENTRY_IO_APIC if entry_len >= 12 => {
                // SAFETY: as above.
                unsafe {
                    madt.io_apics.push(IoApic {
                        address: read_at::<u32>(table, offset + 4) as u64,
                        gsi_base: read_at(table, offset + 8),
                    });
                }
            }
            ENTRY_SOURCE_OVERRIDE if entry_len >= 10 => {
                // SAFETY: as above.
                unsafe {
                    madt.overrides.push(SourceOverride {
                        source: read_at(table, offset + 3),
                        gsi: read_at(table, offset + 4),
                        flags: read_at(table, offset + 8),
                    });
                }
            }
            ENTRY_LOCAL_APIC_ADDRESS_OVERRIDE if entry_len >= 12 => {
                // 64-bit LAPIC base, supersedes the 32-bit field in the header.
                // SAFETY: as above.
                madt.lapic_address = unsafe { read_at(table, offset + 4) };
            }
            _ => {}
        }

        offset += entry_len as usize;
    }

    Some(madt)
}
