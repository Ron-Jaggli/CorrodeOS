//! Checks on the build host: register values, a VBIOS built by hand, and
//! the real firmware files when `scripts/fetch-firmware.sh` has put them in
//! `firmware/`.

use crate::chip::*;
use crate::firmware::{self, BinHeader, HsFirmware, RiscvDescriptor};
use crate::fwsec;
use crate::vbios::{self, DescriptorV3, Vbios};

#[test]
fn a_3060_ti_identifies_as_ga104() {
    // GA104, revision a1: architecture 0x17, implementation 4.
    let boot42 = Boot42::decode(0x174a_1000);
    assert_eq!(boot42.architecture, 0x17);
    assert_eq!(boot42.implementation, 4);
    assert_eq!((boot42.major, boot42.minor), (0xa, 1));
    assert_eq!(boot42.code(), 0x174);
    assert_eq!(boot42.chipset(), Some(Chipset::Ga104));
    assert_eq!(Chipset::Ga104.name(), "ga104");
    assert_eq!(Chipset::Ga104.architecture(), Architecture::Ampere);
    assert!(Architecture::Ampere.boots_by_booter());
    assert_eq!(Architecture::from_code(0x17), Some(Architecture::Ampere));
}

#[test]
fn boot0_splits_its_architecture_across_two_fields() {
    // 0x17 is 1_0111: bit 4 of it lives in bit 8 of the register.
    let boot0 = Boot0::decode(0x0740_01a1);
    assert_eq!(boot0.architecture, 0x27 & 0x1f | 1 << 5);
    let ga104 = Boot0::decode(0x1740_00a1);
    assert_eq!(ga104.architecture, 0x17);
    assert_eq!(ga104.implementation, 4);
    assert_eq!((ga104.major, ga104.minor), (0xa, 1));
    assert!(!ga104.older_than_fermi());
    assert!(Boot0::decode(0x0440_00a1).older_than_fermi());
}

#[test]
fn unknown_chips_are_not_guessed() {
    assert_eq!(Boot42::decode(0x1f00_0000).chipset(), None);
    assert_eq!(Boot42::decode(0xffff_ffff).chipset(), None);
}

/// A VBIOS laid out as Nova expects one: a PCI-AT image with a BIT table,
/// an EFI image to be skipped, and a FWSEC image holding the PMU lookup
/// table and a V3 descriptor with two signatures and some ucode.
struct Rom {
    bytes: Vec<u8>,
    /// Where in the FWSEC data the ucode (after the descriptor) starts.
    ucode_at: usize,
    pci_at_len: usize,
    fwsec_at: usize,
}

const PCI_AT_LEN: usize = 0x800;
const EFI_LEN: usize = 0x400;
const FWSEC_LEN: usize = 0x2000;

fn image_header(out: &mut [u8], code_type: u8, len: usize, last: bool) {
    out[0] = 0x55;
    out[1] = 0xaa;
    out[0x18..0x1a].copy_from_slice(&0x40u16.to_le_bytes());
    let pcir = &mut out[0x40..0x58];
    pcir[0..4].copy_from_slice(b"PCIR");
    pcir[4..6].copy_from_slice(&0x10deu16.to_le_bytes());
    pcir[6..8].copy_from_slice(&0x2489u16.to_le_bytes());
    pcir[0x0a..0x0c].copy_from_slice(&24u16.to_le_bytes());
    pcir[0x10..0x12].copy_from_slice(&((len / 512) as u16).to_le_bytes());
    pcir[0x14] = code_type;
    pcir[0x15] = if last { 0x80 } else { 0 };
}

fn rom() -> Rom {
    let mut bytes = vec![0u8; PCI_AT_LEN + EFI_LEN + FWSEC_LEN];
    let (pci_at, rest) = bytes.split_at_mut(PCI_AT_LEN);
    let (efi, fwsec) = rest.split_at_mut(EFI_LEN);

    image_header(pci_at, vbios::IMAGE_PCI_AT, PCI_AT_LEN, false);
    // The BIT table, with two tokens: something else, then falcon data.
    let bit = 0x100;
    pci_at[bit..bit + 6].copy_from_slice(&[0xff, 0xb8, b'B', b'I', b'T', 0]);
    pci_at[bit + 6..bit + 8].copy_from_slice(&0x0100u16.to_le_bytes());
    pci_at[bit + 8] = 12;
    pci_at[bit + 9] = 6;
    pci_at[bit + 10] = 2;
    pci_at[bit + 12..bit + 18].copy_from_slice(&[0x32, 1, 4, 0, 0, 0]);
    let pointer_at: u16 = 0x200;
    pci_at[bit + 18..bit + 24].copy_from_slice(&[vbios::BIT_FALCON_DATA, 2, 4, 0, 0, 0]);
    pci_at[bit + 22..bit + 24].copy_from_slice(&pointer_at.to_le_bytes());

    image_header(efi, vbios::IMAGE_EFI, EFI_LEN, false);
    image_header(fwsec, vbios::IMAGE_FWSEC, FWSEC_LEN, true);

    // Pointers count from the start of the PCI-AT image, as if FWSEC came
    // straight after it.
    let table = 0x100usize;
    pci_at[pointer_at as usize..pointer_at as usize + 4]
        .copy_from_slice(&((PCI_AT_LEN + table) as u32).to_le_bytes());
    fwsec[table..table + 4].copy_from_slice(&[1, 4, 6, 2]);
    let descriptor = 0x200usize;
    fwsec[table + 4..table + 10].copy_from_slice(&[0x05, 0, 0, 0, 0, 0]);
    fwsec[table + 10] = vbios::APPID_FWSEC_PROD;
    fwsec[table + 12..table + 16]
        .copy_from_slice(&((PCI_AT_LEN + descriptor) as u32).to_le_bytes());

    // The descriptor: version 3, header covering two signatures.
    let header_len = vbios::DESCRIPTOR_V3_LEN + 2 * vbios::SIGNATURE_LEN;
    let d = &mut fwsec[descriptor..];
    d[0..4].copy_from_slice(&(((header_len as u32) << 16) | 3 << 8 | 1).to_le_bytes());
    let words: [u32; 8] = [0x500, 0x80, 0, 0, 0x300, 0, 0, 0x200];
    for (i, word) in words.iter().enumerate() {
        d[4 + 4 * i..8 + 4 * i].copy_from_slice(&word.to_le_bytes());
    }
    d[36..38].copy_from_slice(&0x0400u16.to_le_bytes());
    d[38] = 3;
    d[39] = 2;
    // Signatures for fuse versions 1 and 3.
    d[40..42].copy_from_slice(&0b1010u16.to_le_bytes());
    for (index, fill) in [0xa1u8, 0xa3].iter().enumerate() {
        let at = fwsec::signature_offset(index);
        d[at..at + vbios::SIGNATURE_LEN].fill(*fill);
    }

    // The ucode: 0x300 bytes of code, then 0x200 of data whose application
    // interface table names a DMEM mapper.
    let ucode = descriptor + header_len;
    let code = 0x300;
    let u = &mut fwsec[ucode..];
    // Interface table at the start of data (interface_offset 0).
    u[code..code + 4].copy_from_slice(&[1, 4, 8, 1]);
    u[code + 4..code + 8].copy_from_slice(&4u32.to_le_bytes());
    u[code + 8..code + 12].copy_from_slice(&0x10u32.to_le_bytes());
    // The mapper at data + 0x10, its command buffer at data + 0x80.
    u[code + 0x10..code + 0x14].copy_from_slice(b"DMAP");
    u[code + 0x18..code + 0x1c].copy_from_slice(&0x80u32.to_le_bytes());

    Rom {
        bytes,
        ucode_at: ucode,
        pci_at_len: PCI_AT_LEN,
        fwsec_at: PCI_AT_LEN + EFI_LEN,
    }
}

#[test]
fn a_rom_is_walked_image_by_image() {
    let rom = rom();
    let vbios = Vbios::parse(&rom.bytes).unwrap();
    let images: Vec<vbios::Image> = vbios.images().collect();
    assert_eq!(images.len(), 3);
    assert_eq!(
        images.iter().map(|i| i.code_type).collect::<Vec<_>>(),
        [vbios::IMAGE_PCI_AT, vbios::IMAGE_EFI, vbios::IMAGE_FWSEC]
    );
    assert_eq!(images[2].offset, rom.fwsec_at);
    assert!(images[2].last);
    assert_eq!((images[0].vendor, images[0].device), (0x10de, 0x2489));
    assert_eq!(vbios.bit_version(), 0x0100);
    assert_eq!(vbios.tokens().count(), 2);
    assert_eq!(rom.pci_at_len, vbios.pci_at().len());
}

#[test]
fn fwsec_is_found_past_the_efi_image() {
    let rom = rom();
    let vbios = Vbios::parse(&rom.bytes).unwrap();
    let found = vbios.fwsec().unwrap();
    assert_eq!(found.descriptor, 0x200);
    let desc = found.desc;
    assert_eq!(desc.imem_load_size, 0x300);
    assert_eq!(desc.dmem_load_size, 0x200);
    assert_eq!(desc.pkc_data_offset, 0x80);
    assert_eq!(desc.engine_id_mask, 0x0400);
    assert_eq!((desc.ucode_id, desc.signature_count), (3, 2));
    assert_eq!(desc.header_len, rom.ucode_at - 0x200);

    let mut ucode = vec![0u8; desc.ucode_len()];
    vbios
        .fwsec_bytes(found.descriptor + desc.header_len, &mut ucode)
        .unwrap();
    assert_eq!(&ucode[0x310..0x314], b"DMAP");
}

#[test]
fn a_rom_read_word_by_word_stops_after_the_last_image() {
    let rom = rom();
    let mut reads = 0usize;
    let mut read = |offset: usize| {
        reads += 1;
        rom.bytes
            .get(offset..offset + 4)
            .map_or(0xbad0_bad0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let mut out = vec![0u8; 1 << 20];
    let len = vbios::read_rom(&mut read, &mut out).unwrap();
    assert_eq!(len, rom.bytes.len());
    assert_eq!(&out[..len], &rom.bytes[..]);
    assert!(reads <= len / 4 + 1);
}

#[test]
fn an_ifr_header_moves_the_images() {
    // Version 2: the images start where the word after the fixed data says.
    let rom = rom();
    let mut prom = vec![0u8; 0x1000];
    prom[0..4].copy_from_slice(b"NVGI");
    prom[4..8].copy_from_slice(&((0x20u32 << 16) | 2 << 8).to_le_bytes());
    prom[0x24..0x28].copy_from_slice(&0x1000u32.to_le_bytes());
    prom.extend_from_slice(&rom.bytes);
    let mut read = |offset: usize| {
        prom.get(offset..offset + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    assert_eq!(vbios::rom_offset(&mut read), Ok(0x1000));
    let mut out = vec![0u8; 1 << 20];
    assert_eq!(vbios::read_rom(&mut read, &mut out), Ok(rom.bytes.len()));
}

#[test]
fn broken_roms_are_errors_not_panics() {
    assert_eq!(Vbios::parse(&[]).err(), Some(vbios::Error::NoRomSignature));
    assert_eq!(
        Vbios::parse(&[0u8; 512]).err(),
        Some(vbios::Error::NoRomSignature)
    );
    let good = rom();
    // Every single-byte corruption of the headers, the tables and the
    // descriptor either parses or fails cleanly.
    let interesting = (0..0x60)
        .chain(0x100..0x120)
        .chain(0x200..0x204)
        .chain(PCI_AT_LEN..PCI_AT_LEN + 0x60)
        .chain(good.fwsec_at..good.fwsec_at + 0x60)
        .chain(good.fwsec_at + 0x100..good.fwsec_at + 0x230);
    for at in interesting {
        for value in [0u8, 1, 0x7f, 0x80, 0xff] {
            let mut bytes = good.bytes.clone();
            bytes[at] = value;
            if let Ok(vbios) = Vbios::parse(&bytes) {
                if let Ok(found) = vbios.fwsec() {
                    let mut out = vec![0u8; found.desc.ucode_len().min(1 << 16)];
                    let _ = vbios.fwsec_bytes(found.descriptor + found.desc.header_len, &mut out);
                }
            }
        }
    }
}

#[test]
fn the_frts_command_lands_in_the_dmem_mapper() {
    let rom = rom();
    let vbios = Vbios::parse(&rom.bytes).unwrap();
    let found = vbios.fwsec().unwrap();
    let mut ucode = vec![0u8; found.desc.ucode_len()];
    vbios
        .fwsec_bytes(found.descriptor + found.desc.header_len, &mut ucode)
        .unwrap();

    // 1 MiB of WPR2 at 7 GiB.
    fwsec::patch_frts(&mut ucode, &found.desc, 7 << 30, 1 << 20).unwrap();
    let data = 0x300;
    let word = |at: usize| u32::from_le_bytes(ucode[at..at + 4].try_into().unwrap());
    assert_eq!(word(data + 0x10 + 44), fwsec::COMMAND_FRTS);
    let command = data + 0x80;
    assert_eq!([word(command), word(command + 4)], [1, 24]);
    assert_eq!(word(command + 20), 2);
    let region = command + 24;
    assert_eq!(
        [
            word(region),
            word(region + 4),
            word(region + 8),
            word(region + 12),
            word(region + 16)
        ],
        [1, 20, ((7u64 << 30) >> 12) as u32, 256, 2]
    );
}

#[test]
fn signatures_are_chosen_by_fuse_version() {
    let rom = rom();
    let found = Vbios::parse(&rom.bytes).unwrap().fwsec().unwrap();
    // Versions 1 and 3 are present: index 0 and 1.
    assert_eq!(fwsec::signature_index(&found.desc, 1), Ok(0));
    assert_eq!(fwsec::signature_index(&found.desc, 3), Ok(1));
    assert_eq!(
        fwsec::signature_index(&found.desc, 2),
        Err(fwsec::Error::NoSignature)
    );
    assert_eq!(
        fwsec::signature_index(&found.desc, 40),
        Err(fwsec::Error::NoSignature)
    );
    assert_eq!(fwsec::fuse_version(0), 0);
    assert_eq!(fwsec::fuse_version(0b0100), 3);

    let mut ucode = vec![0u8; found.desc.ucode_len()];
    fwsec::patch_signature(&mut ucode, &found.desc, &[0xa3; vbios::SIGNATURE_LEN]).unwrap();
    assert!(ucode[0x380..0x380 + vbios::SIGNATURE_LEN]
        .iter()
        .all(|b| *b == 0xa3));
    assert!(fwsec::patch_signature(&mut ucode[..0x400], &found.desc, &[0; 384]).is_err());
}

#[test]
fn a_descriptor_that_is_not_version_3_is_refused() {
    let mut bytes = [0u8; vbios::DESCRIPTOR_V3_LEN];
    bytes[1] = 2;
    assert_eq!(
        DescriptorV3::parse(&bytes),
        Err(vbios::Error::BadDescriptor)
    );
}

// --- The real files, when they are there ----------------------------------

const FIRMWARE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../firmware/nvidia/ga104/gsp");

fn real(name: &str) -> Option<Vec<u8>> {
    let path = format!("{FIRMWARE}/{name}-570.144.bin");
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            eprintln!("skipped: {path} is not there (scripts/fetch-firmware.sh)");
            None
        }
    }
}

#[test]
fn real_booters_parse_and_name_sec2() {
    for name in ["booter_load", "booter_unload"] {
        let Some(file) = real(name) else { return };
        let booter = HsFirmware::parse(&file).unwrap();
        eprintln!(
            "{name}: {} byte payload, {} signature(s) of {:?} bytes, fuse {}, engines {:#x}, ucode {}, {} app(s), patch at {:#x}",
            booter.payload().len(),
            booter.signature_count,
            booter.signature_len(),
            booter.fuse_version,
            booter.engine_id_mask,
            booter.ucode_id,
            booter.app_count,
            booter.patch_location
        );
        // Booter runs on SEC2, which is engine bit 0.
        assert_eq!(booter.engine_id_mask & 1, 1);
        assert!(booter.signature_count > 0);
        assert!(booter.app_count >= 1);
        let (offset, len) = booter.app(0).unwrap();
        assert!(len > 0 && (offset + len) as usize <= booter.payload().len());
        assert!(booter.pkc_data_offset().is_some());
        // The last signature is what a card with unprogrammed fuses takes.
        let last = booter.signature_for_fuse(0).unwrap();
        assert_eq!(Some(last.len()), booter.signature_len());
    }
}

#[test]
fn the_real_gsp_bootloader_has_a_riscv_descriptor() {
    let Some(file) = real("bootloader") else {
        return;
    };
    let (bin, desc) = RiscvDescriptor::parse(&file).unwrap();
    eprintln!(
        "bootloader: {} byte payload, code at {:#x}+{:#x}, data at {:#x}+{:#x}, manifest at {:#x}, app version {:#x}",
        bin.data_size,
        desc.monitor_code_offset,
        desc.monitor_code_size,
        desc.monitor_data_offset,
        desc.monitor_data_size,
        desc.manifest_offset,
        desc.app_version
    );
    assert!(desc.monitor_code_size > 0 && desc.manifest_size > 0);
}

#[test]
fn the_real_gsp_image_has_ampere_signatures() {
    let Some(file) = real("gsp") else { return };
    let image = firmware::elf_section(&file, b".fwimage").unwrap();
    let signatures = firmware::elf_section(&file, b".fwsignature_ga10x").unwrap();
    eprintln!(
        "gsp: image {} KiB, ga10x signatures {} bytes",
        image.len() / 1024,
        signatures.len()
    );
    assert!(image.len() > 16 << 20);
    assert!(!signatures.is_empty());
    assert_eq!(
        firmware::elf_section(&file, b".no_such_section"),
        Err(firmware::Error::NoSection)
    );
    // It is ELF, not nvfw.
    assert_eq!(BinHeader::parse(&file), Err(firmware::Error::NotNvfw));
}

#[test]
fn files_cut_short_are_errors_not_panics() {
    let Some(file) = real("booter_load") else {
        return;
    };
    for len in (0..file.len()).step_by(97) {
        let _ = HsFirmware::parse(&file[..len]);
        let _ = RiscvDescriptor::parse(&file[..len]);
        let _ = firmware::elf_section(&file[..len], b".fwimage");
    }
}
