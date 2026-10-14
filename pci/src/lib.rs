//! PCI configuration space, as arithmetic.
//!
//! Everything here is decoding: what the 256 (or 4096) bytes a function
//! exposes mean, how big a BAR is, where its capabilities are, and which of
//! its registers a driver may be allowed to write. None of it touches a
//! machine. Reads and writes go through [`ConfigSpace`], which the kernel
//! implements twice -- memory-mapped ECAM and the legacy port pair -- and the
//! tests implement a third time over an array, which is the point: the part
//! of PCI that is easy to get subtly wrong is exactly the part that can be
//! checked on the build host.
//!
//! A userspace driver uses the same code over its own device's capability,
//! so the walk that finds a virtio structure is the walk the kernel used to
//! find MSI-X.

#![cfg_attr(not(test), no_std)]

pub mod mcfg;

/// Where a function sits: the name the bus knows it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Address {
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Self {
        Self {
            segment,
            bus,
            device,
            function,
        }
    }
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// How wide a configuration access is.
///
/// Carried explicitly rather than built from 32-bit reads and writes. The
/// status register is write-one-to-clear and shares a doubleword with the
/// command register, so "read 32, change 16, write 32" quietly acknowledges
/// every error the device had latched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    Byte = 1,
    Word = 2,
    Dword = 4,
}

impl Width {
    /// From a byte count, as it arrives in a register from ring 3.
    pub const fn from_bytes(bytes: u64) -> Option<Width> {
        match bytes {
            1 => Some(Width::Byte),
            2 => Some(Width::Word),
            4 => Some(Width::Dword),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u16 {
        self as u16
    }

    const fn mask(self) -> u32 {
        match self {
            Width::Byte => 0xff,
            Width::Word => 0xffff,
            Width::Dword => 0xffff_ffff,
        }
    }
}

/// One function's configuration space.
///
/// `offset` must be a multiple of the width. Reads of a register nothing
/// implements return all ones, as the bus itself does.
pub trait ConfigSpace {
    fn read(&self, offset: u16, width: Width) -> u32;
    fn write(&self, offset: u16, width: Width, value: u32);

    fn read8(&self, offset: u16) -> u8 {
        self.read(offset, Width::Byte) as u8
    }
    fn read16(&self, offset: u16) -> u16 {
        self.read(offset, Width::Word) as u16
    }
    fn read32(&self, offset: u16) -> u32 {
        self.read(offset, Width::Dword)
    }
    fn write16(&self, offset: u16, value: u16) {
        self.write(offset, Width::Word, value as u32)
    }
    fn write32(&self, offset: u16, value: u32) {
        self.write(offset, Width::Dword, value)
    }
}

pub const VENDOR_ID: u16 = 0x00;
pub const DEVICE_ID: u16 = 0x02;
pub const COMMAND: u16 = 0x04;
pub const STATUS: u16 = 0x06;
pub const REVISION: u16 = 0x08;
pub const HEADER_TYPE: u16 = 0x0e;
pub const BAR0: u16 = 0x10;
pub const SECONDARY_BUS: u16 = 0x19;
pub const SUBSYSTEM_VENDOR: u16 = 0x2c;
pub const ROM_ADDRESS: u16 = 0x30;
pub const CAPABILITIES: u16 = 0x34;

/// Command register bits.
pub const COMMAND_IO: u16 = 1 << 0;
pub const COMMAND_MEMORY: u16 = 1 << 1;
/// May the device start transfers of its own: DMA.
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
pub const COMMAND_INTX_DISABLE: u16 = 1 << 10;

/// Status bit 4: the capability pointer means something.
const STATUS_CAPABILITIES: u16 = 1 << 4;

pub const CAP_MSI: u8 = 0x05;
pub const CAP_VENDOR: u8 = 0x09;
pub const CAP_EXPRESS: u8 = 0x10;
pub const CAP_MSIX: u8 = 0x11;

/// The size of configuration space reachable through the port pair, and of
/// the part every function has.
pub const LEGACY_SIZE: u16 = 256;
/// The size reachable through ECAM.
pub const EXTENDED_SIZE: u16 = 4096;

/// What a function says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    /// Layout of the rest of the header: 0 a device, 1 a PCI-to-PCI bridge.
    pub kind: u8,
    /// Whether functions 1 to 7 of this device are worth asking about.
    pub multifunction: bool,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
}

impl Header {
    /// Read the identity of a function, or `None` if nothing answers there.
    ///
    /// An absent function is not an error on this bus. Nothing drives the
    /// lines, they float high, and the read comes back all ones -- which is
    /// why 0xffff is the one vendor number that is never assigned.
    pub fn read(config: &impl ConfigSpace) -> Option<Header> {
        let vendor = config.read16(VENDOR_ID);
        if vendor == 0xffff {
            return None;
        }
        let class = config.read32(REVISION);
        let kind = config.read8(HEADER_TYPE);
        let is_device = kind & 0x7f == 0;
        Some(Header {
            vendor,
            device: config.read16(DEVICE_ID),
            revision: class as u8,
            prog_if: (class >> 8) as u8,
            subclass: (class >> 16) as u8,
            class: (class >> 24) as u8,
            kind: kind & 0x7f,
            multifunction: kind & 0x80 != 0,
            // A bridge keeps bus numbers where a device keeps these.
            subsystem_vendor: if is_device {
                config.read16(SUBSYSTEM_VENDOR)
            } else {
                0
            },
            subsystem: if is_device {
                config.read16(SUBSYSTEM_VENDOR + 2)
            } else {
                0
            },
        })
    }

    pub const fn is_bridge(&self) -> bool {
        self.kind == 1
    }

    /// How many base address registers this kind of header has.
    pub const fn bar_count(&self) -> usize {
        match self.kind {
            0 => 6,
            1 => 2,
            _ => 0,
        }
    }

    /// A few words for a human, from the class and subclass.
    pub const fn class_name(&self) -> &'static str {
        match (self.class, self.subclass) {
            (0x01, 0x01) => "IDE controller",
            (0x01, 0x06) => "SATA controller",
            (0x01, 0x08) => "NVMe controller",
            (0x01, _) => "storage controller",
            (0x02, _) => "network controller",
            (0x03, 0x00) => "VGA controller",
            (0x03, _) => "display controller",
            (0x04, 0x03) => "audio device",
            (0x04, _) => "multimedia device",
            (0x05, _) => "memory controller",
            (0x06, 0x00) => "host bridge",
            (0x06, 0x01) => "ISA bridge",
            (0x06, 0x04) => "PCI bridge",
            (0x06, _) => "bridge",
            (0x07, _) => "communication controller",
            (0x08, _) => "system peripheral",
            (0x09, _) => "input device",
            (0x0c, 0x03) => "USB controller",
            (0x0c, 0x05) => "SMBus controller",
            (0x0c, _) => "serial bus controller",
            (0xff, _) => "unclassified device",
            _ => "device",
        }
    }
}

/// One base address register, decoded and sized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Not implemented, or the upper half of the 64-bit BAR before it.
    None,
    Io {
        port: u32,
        size: u32,
    },
    Memory {
        address: u64,
        size: u64,
        prefetchable: bool,
        /// Whether this BAR also occupies the register after it.
        wide: bool,
    },
}

impl Bar {
    /// The memory behind this BAR as `(address, size)`, if it is memory and
    /// the firmware put it somewhere.
    pub const fn memory(&self) -> Option<(u64, u64)> {
        match *self {
            Bar::Memory { address, size, .. } if address != 0 && size != 0 => Some((address, size)),
            _ => None,
        }
    }
}

/// Decode and size every BAR of a function.
///
/// A BAR does not say how big it is. The only way to find out is to write
/// all ones and read back which bits stuck: the device hard-wires the low
/// bits of the address to zero, as many as it takes to cover its size. The
/// original value is put back straight away, and the whole exchange is the
/// one place this crate writes to a device it is only looking at.
///
/// Decoding is left on while this happens, as every other system leaves it:
/// turning it off would silence a device the firmware may be using for its
/// console. The window in which the BAR holds nonsense is two writes long.
pub fn read_bars(config: &impl ConfigSpace, header: &Header) -> [Bar; 6] {
    let mut bars = [Bar::None; 6];
    let count = header.bar_count();
    let mut index = 0;
    while index < count {
        let offset = BAR0 + 4 * index as u16;
        let original = config.read32(offset);
        config.write32(offset, 0xffff_ffff);
        let probed = config.read32(offset);
        config.write32(offset, original);

        if original & 1 != 0 {
            // I/O space: bit 1 is reserved, the address starts at bit 2.
            let mask = probed & !0x3;
            if mask != 0 {
                bars[index] = Bar::Io {
                    port: original & !0x3,
                    // The upper half of a 16-bit decoder reads as zero.
                    size: (!mask).wrapping_add(1) & 0xffff,
                };
            }
            index += 1;
            continue;
        }

        let wide = (original >> 1) & 0x3 == 0x2 && index + 1 < count;
        let prefetchable = original & 0x8 != 0;
        let mut address = (original & !0xf) as u64;
        // A 32-bit BAR decodes nothing above 4 GiB, which is the same as
        // every upper bit having stuck.
        let mut mask = (probed & !0xf) as u64 | 0xffff_ffff_0000_0000;

        if wide {
            let high_offset = offset + 4;
            let high = config.read32(high_offset);
            config.write32(high_offset, 0xffff_ffff);
            let high_probed = config.read32(high_offset);
            config.write32(high_offset, high);
            address |= (high as u64) << 32;
            mask = (mask & 0xffff_ffff) | ((high_probed as u64) << 32);
        }

        // No bit stuck at all: the register is not there.
        let size = (!mask).wrapping_add(1);
        if mask << 32 != 0 || (wide && size != 0) {
            bars[index] = Bar::Memory {
                address,
                size,
                prefetchable,
                wide,
            };
        }
        index += if wide { 2 } else { 1 };
    }
    bars
}

/// The expansion ROM's address and size, for a device that has one.
///
/// Sized the same way as a BAR. Bit 0 is the ROM's own decode enable, which
/// this leaves exactly as it found it: usually off.
pub fn read_rom(config: &impl ConfigSpace, header: &Header) -> Option<(u64, u64)> {
    if header.kind != 0 {
        return None;
    }
    let original = config.read32(ROM_ADDRESS);
    config.write32(ROM_ADDRESS, 0xffff_f800 | (original & 1));
    let probed = config.read32(ROM_ADDRESS);
    config.write32(ROM_ADDRESS, original);
    let mask = probed & 0xffff_f800;
    if mask == 0 {
        return None;
    }
    Some((
        (original & 0xffff_f800) as u64,
        (!mask).wrapping_add(1) as u64,
    ))
}

/// A walk along a function's capability list: `(id, offset)` for each.
///
/// The list is a chain of one-byte pointers inside the device's own
/// registers, so it is exactly as trustworthy as the device. A pointer into
/// the header, or a chain that loops, ends the walk rather than the kernel:
/// there can be at most 48 capabilities in 192 bytes, and no more are
/// followed.
pub struct Capabilities<'a, C: ConfigSpace> {
    config: &'a C,
    next: u8,
    left: u8,
}

impl<'a, C: ConfigSpace> Capabilities<'a, C> {
    pub fn new(config: &'a C) -> Self {
        let present = config.read16(STATUS) & STATUS_CAPABILITIES != 0;
        Self {
            config,
            next: if present {
                config.read8(CAPABILITIES) & !0x3
            } else {
                0
            },
            left: 48,
        }
    }
}

impl<C: ConfigSpace> Iterator for Capabilities<'_, C> {
    type Item = (u8, u8);

    fn next(&mut self) -> Option<(u8, u8)> {
        if self.next < 0x40 || self.left == 0 {
            return None;
        }
        self.left -= 1;
        let offset = self.next;
        let id = self.config.read8(offset as u16);
        self.next = self.config.read8(offset as u16 + 1) & !0x3;
        Some((id, offset))
    }
}

/// Find the first capability with this id.
pub fn find_capability(config: &impl ConfigSpace, id: u8) -> Option<u8> {
    Capabilities::new(config)
        .find(|(found, _)| *found == id)
        .map(|(_, offset)| offset)
}

/// Message signalled interrupts, the older kind: the message lives in
/// configuration space, one address and one data word for the whole device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msi {
    pub offset: u8,
    /// Whether the address register is 64 bits, which moves the data word.
    pub wide: bool,
    /// Whether the capability has per-vector mask bits.
    pub maskable: bool,
}

const MSI_CONTROL_ENABLE: u16 = 1 << 0;
const MSI_CONTROL_WIDE: u16 = 1 << 7;
const MSI_CONTROL_MASKABLE: u16 = 1 << 8;
/// Bits 4 to 6: how many vectors are in use, as a power of two.
const MSI_CONTROL_MULTIPLE: u16 = 0x7 << 4;

impl Msi {
    pub fn read(config: &impl ConfigSpace) -> Option<Msi> {
        let offset = find_capability(config, CAP_MSI)?;
        let control = config.read16(offset as u16 + 2);
        Some(Msi {
            offset,
            wide: control & MSI_CONTROL_WIDE != 0,
            maskable: control & MSI_CONTROL_MASKABLE != 0,
        })
    }

    /// Bytes of configuration space this capability occupies.
    pub const fn span(&self) -> u16 {
        match (self.wide, self.maskable) {
            (false, false) => 10,
            (true, false) => 14,
            (false, true) => 20,
            (true, true) => 24,
        }
    }

    /// Point the device's one message at `address` with `data`, and turn it
    /// on. One vector: a device that could use more is told to use one.
    pub fn enable(&self, config: &impl ConfigSpace, address: u64, data: u16) {
        let base = self.offset as u16;
        let control = config.read16(base + 2) & !(MSI_CONTROL_ENABLE | MSI_CONTROL_MULTIPLE);
        // Off while the message is half written.
        config.write16(base + 2, control);
        config.write32(base + 4, address as u32);
        if self.wide {
            config.write32(base + 8, (address >> 32) as u32);
            config.write16(base + 12, data);
        } else {
            config.write16(base + 8, data);
        }
        config.write16(base + 2, control | MSI_CONTROL_ENABLE);
    }

    pub fn disable(&self, config: &impl ConfigSpace) {
        let base = self.offset as u16;
        let control = config.read16(base + 2);
        config.write16(base + 2, control & !MSI_CONTROL_ENABLE);
    }
}

/// Message signalled interrupts, the newer kind: a table of messages in the
/// device's own memory, behind one of its BARs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiX {
    pub offset: u8,
    /// Entries in the table.
    pub entries: u16,
    /// Which BAR the table is behind, and how far into it.
    pub table_bar: u8,
    pub table_offset: u32,
    /// The same for the pending bits.
    pub pending_bar: u8,
    pub pending_offset: u32,
}

const MSIX_CONTROL_ENABLE: u16 = 1 << 15;
/// Masks every vector at once, whatever the table says.
const MSIX_CONTROL_MASK_ALL: u16 = 1 << 14;

/// Bytes per entry of an MSI-X table: address low, address high, data,
/// and a control word whose bit 0 masks the entry.
pub const MSIX_ENTRY_LEN: u64 = 16;
pub const MSIX_ENTRY_MASKED: u32 = 1;

impl MsiX {
    pub fn read(config: &impl ConfigSpace) -> Option<MsiX> {
        let offset = find_capability(config, CAP_MSIX)?;
        let base = offset as u16;
        let control = config.read16(base + 2);
        let table = config.read32(base + 4);
        let pending = config.read32(base + 8);
        Some(MsiX {
            offset,
            entries: (control & 0x7ff) + 1,
            table_bar: (table & 0x7) as u8,
            table_offset: table & !0x7,
            pending_bar: (pending & 0x7) as u8,
            pending_offset: pending & !0x7,
        })
    }

    /// Bytes of configuration space this capability occupies.
    pub const fn span(&self) -> u16 {
        12
    }

    /// Bytes of the BAR the table itself occupies.
    pub const fn table_len(&self) -> u64 {
        self.entries as u64 * MSIX_ENTRY_LEN
    }

    /// Turn MSI-X on. Which entries then fire is the table's business.
    pub fn enable(&self, config: &impl ConfigSpace) {
        let base = self.offset as u16;
        let control = config.read16(base + 2) & !MSIX_CONTROL_MASK_ALL;
        config.write16(base + 2, control | MSIX_CONTROL_ENABLE);
    }

    pub fn disable(&self, config: &impl ConfigSpace) {
        let base = self.offset as u16;
        let control = config.read16(base + 2);
        config.write16(base + 2, control & !MSIX_CONTROL_ENABLE);
    }
}

/// What a driver may not write in its own device's configuration space.
///
/// Holding a device is not holding the bus. Three kinds of register decide
/// where a device sits among the others, and those stay with the kernel:
///
/// * the BARs and the ROM address, because moving a device's memory is how
///   it gets put on top of someone else's;
/// * the interrupt capabilities, because the message in them names a vector
///   on a CPU, and a driver that could write one could raise any interrupt
///   in the machine;
/// * everything in a bridge, whose registers are the shape of the bus.
///
/// What is left -- the command register, and every device-specific byte --
/// is the driver's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WritePolicy {
    bridge: bool,
    bars: usize,
    msi: Option<(u16, u16)>,
    msix: Option<(u16, u16)>,
}

impl WritePolicy {
    pub fn new(header: &Header, msi: Option<Msi>, msix: Option<MsiX>) -> Self {
        Self {
            bridge: header.kind != 0,
            bars: header.bar_count(),
            msi: msi.map(|msi| (msi.offset as u16, msi.span())),
            msix: msix.map(|msix| (msix.offset as u16, msix.span())),
        }
    }

    /// Whether a write of `width` at `offset` is the driver's to make.
    pub fn allows(&self, offset: u16, width: Width) -> bool {
        if self.bridge {
            return false;
        }
        let end = offset + width.bytes();
        let overlaps = |start: u16, len: u16| offset < start + len && start < end;

        // Identity, class and header type are read-only on the device anyway;
        // refusing them makes a mistake visible instead of silently ignored.
        if overlaps(VENDOR_ID, 4) || overlaps(REVISION, 8) {
            return false;
        }
        if overlaps(BAR0, 4 * self.bars as u16) || overlaps(ROM_ADDRESS, 4) {
            return false;
        }
        // The capability pointer and the reserved bytes after it.
        if overlaps(CAPABILITIES, 8) {
            return false;
        }
        for (start, len) in [self.msi, self.msix].into_iter().flatten() {
            if overlaps(start, len) {
                return false;
            }
        }
        true
    }
}

/// Whether an access of `width` at `offset` is well formed for a
/// configuration space of `size` bytes: aligned, and inside it.
pub const fn in_bounds(offset: u64, width: Width, size: u16) -> bool {
    let bytes = width.bytes() as u64;
    // Compared before it is added to: the offset is whatever ring 3 said.
    offset < size as u64 && offset.is_multiple_of(bytes) && offset + bytes <= size as u64
}

/// Keep only the bits a register of `width` holds.
pub const fn truncate(value: u64, width: Width) -> u32 {
    value as u32 & width.mask()
}

/// The x86 MSI message that raises `vector` on the CPU whose local APIC has
/// `apic_id`: fixed delivery, edge triggered, physical destination.
///
/// The address is not memory. Writes to 0xFEExxxxx are what a local APIC
/// listens for, and bits 12 to 19 pick which one.
pub const fn x86_message(apic_id: u8, vector: u8) -> (u64, u16) {
    (0xfee0_0000 | ((apic_id as u64) << 12), vector as u16)
}

#[cfg(test)]
mod tests;
