//! A device made of an array, and the questions asked of it.
//!
//! The model below does what the specification says a function does and no
//! more: BAR bits below the size are hard-wired to zero, the status register
//! clears the bits written as one, and unimplemented registers read as zero.

use super::*;
use std::cell::RefCell;

struct Fake {
    bytes: RefCell<[u8; 256]>,
    /// Size of each BAR's decoder, zero for none. A 64-bit BAR is two
    /// entries: the size, then `u64::MAX` to mark the upper half.
    bar_sizes: [u64; 6],
    rom_size: u32,
    /// Every write, as it arrived.
    log: RefCell<Vec<(u16, Width, u32)>>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            bytes: RefCell::new([0; 256]),
            bar_sizes: [0; 6],
            rom_size: 0,
            log: RefCell::new(Vec::new()),
        }
    }

    fn set(&self, offset: u16, bytes: &[u8]) {
        self.bytes.borrow_mut()[offset as usize..offset as usize + bytes.len()]
            .copy_from_slice(bytes);
    }

    fn set32(&self, offset: u16, value: u32) {
        self.set(offset, &value.to_le_bytes());
    }

    fn raw32(&self, offset: u16) -> u32 {
        let bytes = self.bytes.borrow();
        let at = offset as usize;
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// The bits of a BAR register that hold whatever is written to them.
    fn bar_writable(&self, index: usize) -> u32 {
        let size = self.bar_sizes[index];
        if size == u64::MAX {
            // Upper half: writable above the size of the BAR below it.
            let low = self.bar_sizes[index - 1];
            return (!(low - 1) >> 32) as u32;
        }
        if size == 0 {
            return 0;
        }
        !(size - 1) as u32 & !0xf
    }
}

impl ConfigSpace for Fake {
    fn read(&self, offset: u16, width: Width) -> u32 {
        assert_eq!(offset % width.bytes(), 0, "misaligned read at {offset:#x}");
        let bytes = self.bytes.borrow();
        let mut value = 0u32;
        for i in 0..width.bytes() as usize {
            value |= (bytes[offset as usize + i] as u32) << (8 * i);
        }
        value
    }

    fn write(&self, offset: u16, width: Width, value: u32) {
        assert_eq!(offset % width.bytes(), 0, "misaligned write at {offset:#x}");
        self.log.borrow_mut().push((offset, width, value));

        if (BAR0..BAR0 + 24).contains(&offset) {
            assert_eq!(width, Width::Dword);
            let index = ((offset - BAR0) / 4) as usize;
            let writable = self.bar_writable(index);
            let kept = self.raw32(offset) & !writable;
            self.set32(offset, kept | (value & writable));
            return;
        }
        if offset == ROM_ADDRESS {
            let writable = if self.rom_size == 0 {
                0
            } else {
                !(self.rom_size - 1) & 0xffff_f800 | 1
            };
            let kept = self.raw32(offset) & !writable;
            self.set32(offset, kept | (value & writable));
            return;
        }
        if offset == STATUS {
            // Write one to clear.
            let now = self.read(STATUS, Width::Word) & !value;
            self.set(STATUS, &(now as u16).to_le_bytes());
            return;
        }
        let bytes = value.to_le_bytes();
        self.set(offset, &bytes[..width.bytes() as usize]);
    }
}

/// Something shaped like a display card: a 16 MiB 32-bit BAR, a 256 MiB
/// 64-bit prefetchable one above 4 GiB, an I/O BAR, a ROM, MSI and MSI-X.
fn card() -> Fake {
    let mut fake = Fake::new();
    fake.set(VENDOR_ID, &[0xde, 0x10, 0x89, 0x24]);
    fake.set(STATUS, &[0x10, 0x80]);
    fake.set(REVISION, &[0xa1, 0x00, 0x00, 0x03]);
    fake.set(HEADER_TYPE, &[0x80]);
    fake.set(SUBSYSTEM_VENDOR, &[0x62, 0x14, 0x90, 0x39]);

    fake.bar_sizes = [16 << 20, 256 << 20, u64::MAX, 32 << 20, u64::MAX, 0];
    fake.set32(BAR0, 0xfb00_0000);
    fake.set32(BAR0 + 4, 0x0000_000c);
    fake.set32(BAR0 + 8, 0x0000_0060);
    fake.set32(BAR0 + 12, 0x1000_000c);
    fake.set32(BAR0 + 16, 0x0000_0060);
    fake.rom_size = 512 << 10;
    fake.set32(ROM_ADDRESS, 0xfc00_0000);

    // Capabilities: MSI at 0x68, vendor at 0x78, MSI-X at 0xb4.
    fake.set(CAPABILITIES, &[0x68]);
    fake.set(0x68, &[CAP_MSI, 0x78, 0x80, 0x00]);
    fake.set(0x78, &[CAP_VENDOR, 0xb4, 0x14, 0x00]);
    fake.set(0xb4, &[CAP_MSIX, 0x00, 0x05, 0x00]);
    fake.set32(0xb8, 0x000b_0000);
    fake.set32(0xbc, 0x000c_0003);
    fake
}

#[test]
fn nothing_answers_as_all_ones() {
    let fake = Fake::new();
    fake.set(VENDOR_ID, &[0xff, 0xff]);
    assert_eq!(Header::read(&fake), None);
}

#[test]
fn a_header_says_what_the_device_is() {
    let header = Header::read(&card()).unwrap();
    assert_eq!((header.vendor, header.device), (0x10de, 0x2489));
    assert_eq!((header.class, header.subclass, header.prog_if), (3, 0, 0));
    assert_eq!(header.revision, 0xa1);
    assert_eq!(header.kind, 0);
    assert!(header.multifunction);
    assert_eq!(
        (header.subsystem_vendor, header.subsystem),
        (0x1462, 0x3990)
    );
    assert_eq!(header.class_name(), "VGA controller");
}

#[test]
fn bars_are_sized_by_the_bits_that_do_not_stick() {
    let fake = card();
    let header = Header::read(&fake).unwrap();
    let bars = read_bars(&fake, &header);

    assert_eq!(
        bars[0],
        Bar::Memory {
            address: 0xfb00_0000,
            size: 16 << 20,
            prefetchable: false,
            wide: false,
        }
    );
    assert_eq!(
        bars[1],
        Bar::Memory {
            address: 0x60_0000_0000,
            size: 256 << 20,
            prefetchable: true,
            wide: true,
        }
    );
    // The upper half of a 64-bit BAR is not a BAR.
    assert_eq!(bars[2], Bar::None);
    assert_eq!(
        bars[3],
        Bar::Memory {
            address: 0x60_1000_0000,
            size: 32 << 20,
            prefetchable: true,
            wide: true,
        }
    );
    assert_eq!(bars[4], Bar::None);
    assert_eq!(bars[5], Bar::None);
}

#[test]
fn sizing_puts_every_bar_back() {
    let fake = card();
    let before: Vec<u32> = (0..6).map(|i| fake.raw32(BAR0 + 4 * i)).collect();
    let header = Header::read(&fake).unwrap();
    read_bars(&fake, &header);
    read_rom(&fake, &header);
    let after: Vec<u32> = (0..6).map(|i| fake.raw32(BAR0 + 4 * i)).collect();
    assert_eq!(before, after);
    assert_eq!(fake.raw32(ROM_ADDRESS), 0xfc00_0000);
}

#[test]
fn sizing_never_turns_the_rom_on() {
    let fake = card();
    let header = Header::read(&fake).unwrap();
    assert_eq!(read_rom(&fake, &header), Some((0xfc00_0000, 512 << 10)));
    let enabled = fake
        .log
        .borrow()
        .iter()
        .any(|(offset, _, value)| *offset == ROM_ADDRESS && value & 1 != 0);
    assert!(!enabled);
}

#[test]
fn an_io_bar_is_ports_not_memory() {
    let mut fake = Fake::new();
    fake.set(VENDOR_ID, &[0x86, 0x80, 0x22, 0x29]);
    fake.bar_sizes[4] = 32;
    fake.set32(BAR0 + 16, 0xc041);
    // The model keeps the low four bits; an I/O BAR hard-wires bit 0 to one.
    let header = Header::read(&fake).unwrap();
    let bars = read_bars(&fake, &header);
    assert_eq!(
        bars[4],
        Bar::Io {
            port: 0xc040,
            size: 32,
        }
    );
    assert_eq!(bars[4].memory(), None);
}

#[test]
fn a_bridge_has_two_bars_and_no_subsystem() {
    let fake = Fake::new();
    fake.set(VENDOR_ID, &[0x86, 0x80, 0x44, 0x24]);
    fake.set(REVISION, &[0x00, 0x00, 0x04, 0x06]);
    fake.set(HEADER_TYPE, &[0x01]);
    // Where a device keeps its subsystem, a bridge keeps something else.
    fake.set(SUBSYSTEM_VENDOR, &[0xaa, 0xbb, 0xcc, 0xdd]);
    let header = Header::read(&fake).unwrap();
    assert!(header.is_bridge());
    assert_eq!(header.bar_count(), 2);
    assert_eq!(header.subsystem_vendor, 0);
    assert_eq!(read_rom(&fake, &header), None);
}

#[test]
fn capabilities_are_found_in_order() {
    let fake = card();
    let found: Vec<(u8, u8)> = Capabilities::new(&fake).collect();
    assert_eq!(
        found,
        [(CAP_MSI, 0x68), (CAP_VENDOR, 0x78), (CAP_MSIX, 0xb4)]
    );
    assert_eq!(find_capability(&fake, CAP_VENDOR), Some(0x78));
    assert_eq!(find_capability(&fake, CAP_EXPRESS), None);
}

#[test]
fn a_device_without_the_status_bit_has_no_capabilities() {
    let fake = card();
    fake.set(STATUS, &[0x00, 0x00]);
    assert_eq!(Capabilities::new(&fake).count(), 0);
}

#[test]
fn a_capability_list_that_loops_still_ends() {
    let fake = card();
    // The last capability points back at the first.
    fake.set(0xb5, &[0x68]);
    assert_eq!(Capabilities::new(&fake).count(), 48);
}

#[test]
fn a_capability_pointer_into_the_header_is_not_followed() {
    let fake = card();
    fake.set(CAPABILITIES, &[0x10]);
    assert_eq!(Capabilities::new(&fake).count(), 0);
}

#[test]
fn msi_is_written_with_the_enable_last() {
    let fake = card();
    let msi = Msi::read(&fake).unwrap();
    assert!(msi.wide && !msi.maskable);
    assert_eq!(msi.span(), 14);

    fake.log.borrow_mut().clear();
    let (address, data) = x86_message(3, 0x52);
    msi.enable(&fake, address, data);

    assert_eq!(fake.raw32(0x6c), 0xfee0_3000);
    assert_eq!(fake.raw32(0x70), 0);
    assert_eq!(fake.read16(0x74), 0x52);
    assert_eq!(fake.read16(0x6a) & 1, 1);

    let log = fake.log.borrow();
    let last = log.last().unwrap();
    assert_eq!((last.0, last.2 & 1), (0x6a, 1));
    // Every write before the last left it off.
    assert!(log[..log.len() - 1]
        .iter()
        .all(|(offset, _, value)| *offset != 0x6a || value & 1 == 0));

    drop(log);
    msi.disable(&fake);
    assert_eq!(fake.read16(0x6a) & 1, 0);
}

#[test]
fn narrow_msi_keeps_its_data_closer() {
    let fake = card();
    fake.set(0x6a, &[0x00, 0x00]);
    let msi = Msi::read(&fake).unwrap();
    assert!(!msi.wide);
    assert_eq!(msi.span(), 10);
    msi.enable(&fake, 0xfee0_1000, 0x51);
    assert_eq!(fake.read16(0x70), 0x51);
}

#[test]
fn msix_says_where_its_table_is() {
    let fake = card();
    let msix = MsiX::read(&fake).unwrap();
    assert_eq!(msix.entries, 6);
    assert_eq!((msix.table_bar, msix.table_offset), (0, 0xb_0000));
    assert_eq!((msix.pending_bar, msix.pending_offset), (3, 0xc_0000));
    assert_eq!(msix.table_len(), 96);

    msix.enable(&fake);
    assert_eq!(fake.read16(0xb6) & 0xc000, 0x8000);
    msix.disable(&fake);
    assert_eq!(fake.read16(0xb6) & 0x8000, 0);
}

#[test]
fn the_command_register_is_the_drivers_and_the_bars_are_not() {
    let fake = card();
    let header = Header::read(&fake).unwrap();
    let policy = WritePolicy::new(&header, Msi::read(&fake), MsiX::read(&fake));

    assert!(policy.allows(COMMAND, Width::Word));
    assert!(policy.allows(STATUS, Width::Word));
    // Device-specific registers, including the vendor capability.
    assert!(policy.allows(0x40, Width::Dword));
    assert!(policy.allows(0x7c, Width::Dword));
    assert!(policy.allows(0xfc, Width::Dword));

    for bar in 0..6 {
        assert!(!policy.allows(BAR0 + 4 * bar, Width::Dword));
        assert!(!policy.allows(BAR0 + 4 * bar + 3, Width::Byte));
    }
    assert!(!policy.allows(ROM_ADDRESS, Width::Dword));
    assert!(!policy.allows(VENDOR_ID, Width::Word));
    assert!(!policy.allows(CAPABILITIES, Width::Byte));
    // A wide write that starts in the command register and ends in the BARs
    // cannot exist, but one that starts before a capability and runs into it
    // can.
    assert!(!policy.allows(0x68, Width::Dword));
    assert!(!policy.allows(0x74, Width::Word));
    assert!(policy.allows(0x76, Width::Word));
    assert!(!policy.allows(0xb4, Width::Byte));
    assert!(!policy.allows(0xbc, Width::Dword));
    assert!(policy.allows(0xc0, Width::Dword));
}

#[test]
fn nothing_in_a_bridge_is_a_drivers_to_write() {
    let fake = Fake::new();
    fake.set(HEADER_TYPE, &[0x01]);
    let header = Header::read(&fake).unwrap();
    let policy = WritePolicy::new(&header, None, None);
    assert!(!policy.allows(COMMAND, Width::Word));
    assert!(!policy.allows(SECONDARY_BUS - 1, Width::Dword));
}

#[test]
fn accesses_are_aligned_and_inside() {
    assert!(in_bounds(0xfc, Width::Dword, LEGACY_SIZE));
    assert!(!in_bounds(0xfd, Width::Dword, LEGACY_SIZE));
    assert!(!in_bounds(0x100, Width::Byte, LEGACY_SIZE));
    assert!(in_bounds(0x100, Width::Byte, EXTENDED_SIZE));
    assert!(!in_bounds(0xfff, Width::Word, EXTENDED_SIZE));
    assert!(!in_bounds(u64::MAX, Width::Byte, EXTENDED_SIZE));
    assert_eq!(Width::from_bytes(2), Some(Width::Word));
    assert_eq!(Width::from_bytes(3), None);
    assert_eq!(truncate(0x1234_5678_9abc_def0, Width::Word), 0xdef0);
}

#[test]
fn status_is_not_clobbered_by_a_command_write() {
    // The reason widths are explicit: status is write-one-to-clear and sits
    // in the same doubleword as command.
    let fake = card();
    fake.set(STATUS, &[0x10, 0xf9]);
    fake.write16(COMMAND, COMMAND_MEMORY | COMMAND_BUS_MASTER);
    assert_eq!(fake.read16(STATUS), 0xf910);
    assert_eq!(fake.read16(COMMAND), 0x0006);
}

#[test]
fn mcfg_entries_are_read_unaligned() {
    let mut table = vec![0u8; 44];
    table.extend_from_slice(&0xb000_0000u64.to_le_bytes());
    table.extend_from_slice(&[0, 0, 0, 255, 0, 0, 0, 0]);
    table.extend_from_slice(&0x40_0000_0000u64.to_le_bytes());
    table.extend_from_slice(&[1, 0, 32, 63, 0, 0, 0, 0]);
    // A trailing fragment is not an entry.
    table.extend_from_slice(&[0xaa; 7]);

    let found: Vec<mcfg::Allocation> = mcfg::allocations(&table).collect();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].base, 0xb000_0000);
    assert_eq!((found[0].first_bus, found[0].last_bus), (0, 255));
    assert_eq!(found[1].segment, 1);

    assert_eq!(found[0].address(0, 0, 0), Some(0xb000_0000));
    assert_eq!(
        found[0].address(1, 2, 3),
        Some(0xb000_0000 + 0x10_0000 + 0x1_0000 + 0x3000)
    );
    assert_eq!(found[1].address(31, 0, 0), None);
    assert_eq!(
        found[1].address(32, 0, 0),
        Some(0x40_0000_0000 + (32 << 20))
    );
    assert_eq!(found[0].address(0, 32, 0), None);

    assert_eq!(mcfg::allocations(&table[..20]).count(), 0);
}
