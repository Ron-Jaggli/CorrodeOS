//! The ACPI MCFG table: where configuration space is in memory.
//!
//! PCI Express maps every function's 4 KiB of configuration space at
//! `base + (bus << 20 | device << 15 | function << 12)`. The table says where
//! `base` is, for which buses. It is forty-four bytes of header and then
//! sixteen-byte entries, none of it aligned to anything.

/// One run of buses and where their configuration space starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    /// Physical address of bus 0's configuration space, whether or not bus 0
    /// is in the range.
    pub base: u64,
    pub segment: u16,
    pub first_bus: u8,
    pub last_bus: u8,
}

impl Allocation {
    /// Physical address of one function's configuration space, if its bus
    /// is one of these.
    pub fn address(&self, bus: u8, device: u8, function: u8) -> Option<u64> {
        if bus < self.first_bus || bus > self.last_bus || device > 31 || function > 7 {
            return None;
        }
        Some(self.base + ((bus as u64) << 20 | (device as u64) << 15 | (function as u64) << 12))
    }
}

const HEADER_LEN: usize = 44;
const ENTRY_LEN: usize = 16;

/// The allocations in an MCFG table, given the whole table as bytes.
///
/// A table too short to hold its own header yields nothing, and a trailing
/// partial entry is ignored.
pub fn allocations(table: &[u8]) -> impl Iterator<Item = Allocation> + '_ {
    let entries = table.get(HEADER_LEN..).unwrap_or(&[]);
    entries
        .as_chunks::<ENTRY_LEN>()
        .0
        .iter()
        .map(|entry| Allocation {
            base: u64::from_le_bytes([
                entry[0], entry[1], entry[2], entry[3], entry[4], entry[5], entry[6], entry[7],
            ]),
            segment: u16::from_le_bytes([entry[8], entry[9]]),
            first_bus: entry[10],
            last_bus: entry[11],
        })
}
