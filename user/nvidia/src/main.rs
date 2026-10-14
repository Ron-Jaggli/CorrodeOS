//! NVIDIA, stage one: find out what is there, and change nothing.
//!
//! **Written, never run on a card.** QEMU has no NVIDIA GPU and the one in
//! the development machine is driving its desktop. The decoding this uses
//! is tested on the build host (`corrode-nv`); the firmware half of this
//! program runs in every QEMU boot that carries the firmware files; the
//! card half has never executed.
//!
//! Read-only is not a promise this program makes, it is what it was given:
//! its device and BAR0 capabilities carry READ alone, so its configuration
//! space cannot be written and BAR0 is mapped read-only. A write here would
//! fault, and stop this task.
//!
//! What it does with a card:
//!
//! 1. reads the configuration header, and stops if memory decoding is off
//!    (turning it on would be a change);
//! 2. maps BAR0 and reads `NV_PMC_BOOT_0` and `NV_PMC_BOOT_42`, the chip's
//!    identity, stopping at all-ones, which is a card not answering;
//! 3. reads how far the card's own boot firmware got and how much video
//!    memory it found;
//! 4. copies the VBIOS out through the PROM window and walks it as far as
//!    the FWSEC descriptor the next stage will need.

#![no_std]
#![no_main]

use corrode_nv::chip::{Boot0, Boot42};
use corrode_nv::firmware::{elf_section, HsFirmware, RiscvDescriptor};
use corrode_nv::{regs, vbios};
use corrode_rt::abi::{kind, nvidia};
use corrode_rt::device::{Config, Mmio};
use corrode_rt::pci::{ConfigSpace, COMMAND, COMMAND_MEMORY, VENDOR_ID};
use corrode_rt::{identify, map, println, send, Message};

corrode_rt::entry!(main);

fn report(label: u64, words: [u64; 4]) {
    let _ = send(nvidia::REPORT, Message::new(label, words));
}

/// A blob we hold, as bytes, if we hold it.
fn blob(slot: usize) -> Option<&'static [u8]> {
    if identify(slot).map(|(k, _)| k) != Ok(kind::BLOB) {
        return None;
    }
    let mapping = map(slot).ok()?;
    // SAFETY: the kernel mapped `len` bytes here read-only for the life of
    // this task, and no holder of a blob can write it.
    Some(unsafe { core::slice::from_raw_parts(mapping.address, mapping.len) })
}

/// Parse whatever firmware files we were given, and say what is in them.
fn firmware() {
    let mut parsed = 0u64;
    let mut gsp_kib = 0u64;
    let mut booter_signatures = 0u64;

    for (bit, slot, name) in [
        (0, nvidia::BOOTER_LOAD, "booter_load"),
        (1, nvidia::BOOTER_UNLOAD, "booter_unload"),
    ] {
        let Some(file) = blob(slot) else { continue };
        match HsFirmware::parse(file) {
            Ok(booter) => {
                parsed |= 1 << bit;
                booter_signatures = booter.signature_count as u64;
                println!(
                    "{}: {} KiB for SEC2 (engines {:#x}, ucode {}), {} signature(s), fuse version {}",
                    name,
                    booter.payload().len() / 1024,
                    booter.engine_id_mask,
                    booter.ucode_id,
                    booter.signature_count,
                    booter.fuse_version
                );
            }
            Err(error) => println!("{}: not a Booter image: {:?}", name, error),
        }
    }
    if let Some(file) = blob(nvidia::BOOTLOADER) {
        match RiscvDescriptor::parse(file) {
            Ok((_, desc)) => {
                parsed |= 1 << 2;
                println!(
                    "bootloader: RISC-V, {} KiB code, {} KiB data",
                    desc.monitor_code_size / 1024,
                    desc.monitor_data_size / 1024
                );
            }
            Err(error) => println!("bootloader: not a RISC-V image: {:?}", error),
        }
    }
    if let Some(file) = blob(nvidia::GSP) {
        match (
            elf_section(file, b".fwimage"),
            elf_section(file, b".fwsignature_ga10x"),
        ) {
            (Ok(image), Ok(signatures)) => {
                parsed |= 1 << 3;
                gsp_kib = image.len() as u64 / 1024;
                println!(
                    "gsp: GSP-RM image {} MiB, {} bytes of Ampere signatures",
                    image.len() >> 20,
                    signatures.len()
                );
            }
            _ => println!("gsp: no .fwimage or no Ampere signatures"),
        }
    }
    report(
        nvidia::LABEL_FIRMWARE,
        [parsed, gsp_kib, booter_signatures, 0],
    );
}

/// Look at the card, if we were given one.
fn card() {
    if identify(nvidia::DEVICE).map(|(k, _)| k) != Ok(kind::PCI_DEVICE) {
        println!("no card was handed over; nothing more to look at");
        return;
    }
    let config = Config(nvidia::DEVICE);
    let id = config.read32(VENDOR_ID);
    let command = config.read16(COMMAND);
    println!(
        "card {:04x}:{:04x}, command {:#06x}",
        id & 0xffff,
        id >> 16,
        command
    );
    if command & COMMAND_MEMORY == 0 {
        println!("memory decoding is off; turning it on would be a change, so stopping");
        return;
    }

    let bar0 = match Mmio::map(nvidia::BAR0) {
        Ok(bar0) => bar0,
        Err(error) => {
            println!("no BAR0: {:?}", error);
            return;
        }
    };
    let boot0 = bar0.read32(regs::PMC_BOOT_0);
    if boot0 == regs::DEAD {
        println!("BOOT_0 reads all ones: the card is not answering");
        return;
    }
    let boot42 = Boot42::decode(bar0.read32(regs::PMC_BOOT_42));
    let decoded = Boot0::decode(boot0);
    match boot42.chipset() {
        Some(chipset) => println!(
            "{} ({:?}), revision {:x}{:x}; BOOT_0 {:#010x}",
            chipset.name(),
            chipset.architecture(),
            boot42.major,
            boot42.minor,
            boot0
        ),
        None => println!(
            "unknown chip: architecture {:#x} implementation {:#x}; BOOT_0 {:#010x}",
            boot42.architecture, boot42.implementation, decoded.raw
        ),
    }

    // How far did the card's own boot get? Readable only once FWSEC has
    // lowered the protection on it.
    let readable = bar0.read32(regs::GFW_BOOT_PRIV_LEVEL_MASK) & 1 != 0;
    if readable {
        let progress = bar0.read32(regs::GFW_BOOT) & 0xff;
        println!(
            "boot firmware progress {:#04x}{}",
            progress,
            if progress == 0xff { ", done" } else { "" }
        );
    }
    let fb_mib = bar0.read32(regs::USABLE_FB_SIZE_IN_MB);
    println!(
        "{} MiB of video memory, as the boot firmware found it",
        fb_mib
    );
    report(
        nvidia::LABEL_CARD,
        [
            boot0 as u64,
            boot42.raw as u64,
            boot42.code() as u64,
            fb_mib as u64,
        ],
    );

    // The VBIOS, through the PROM window.
    let Ok(rom) = map(nvidia::ROM) else {
        println!("no memory to copy the VBIOS into");
        return;
    };
    // SAFETY: mapped writable for the life of this task, ours alone.
    let rom = unsafe { core::slice::from_raw_parts_mut(rom.address, rom.len) };
    let window = regs::PROM_SCAN_MAX.min(bar0.len().saturating_sub(regs::PROM));
    let mut read = |offset: usize| {
        if offset + 4 > window {
            return 0;
        }
        bar0.read32(regs::PROM + offset)
    };
    let len = match vbios::read_rom(&mut read, rom) {
        Ok(len) => len,
        Err(error) => {
            println!("could not read the VBIOS: {:?}", error);
            return;
        }
    };
    let parsed = match vbios::Vbios::parse(&rom[..len]) {
        Ok(parsed) => parsed,
        Err(error) => {
            println!(
                "{} KiB of VBIOS that does not parse: {:?}",
                len / 1024,
                error
            );
            return;
        }
    };
    for image in parsed.images() {
        println!(
            "vbios image at {:#x}: {} KiB, type {:#04x}{}",
            image.offset,
            image.len / 1024,
            image.code_type,
            if image.last { ", last" } else { "" }
        );
    }
    let fwsec = parsed.fwsec();
    match fwsec {
        Ok(found) => println!(
            "FWSEC: {} bytes of code, {} of data, {} signature(s), engines {:#x}",
            found.desc.imem_load_size,
            found.desc.dmem_load_size,
            found.desc.signature_count,
            found.desc.engine_id_mask
        ),
        Err(error) => println!("no FWSEC: {:?}", error),
    }
    report(
        nvidia::LABEL_VBIOS,
        [
            len as u64,
            parsed.images().count() as u64,
            parsed.bit_version() as u64,
            fwsec.map_or(u64::MAX, |found| found.descriptor as u64),
        ],
    );
}

fn main() {
    corrode_rt::log::set(nvidia::LOG);
    firmware();
    card();
    report(nvidia::LABEL_DONE, [0; 4]);
}
