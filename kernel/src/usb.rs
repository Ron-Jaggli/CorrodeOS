//! The USB host controller: an xHCI controller, driven by a ring 3 task,
//! `xhci`, that holds the controller, its interrupt, memory for its rings
//! and a send-only endpoint to the compositor -- the one the PS/2 driver
//! sends to -- and nothing else. See `corrode_abi::xhci`.
//!
//! No controller is the usual case on QEMU's default machine, and fine.

use corrode_abi::xhci;

use crate::cap::{self, Rights};
use crate::{irq, memory, modules, pci, println, task};

/// Slots in the kernel's capability space: clear of the disk's (82-91) and
/// of the windows' surfaces (96 on).
const USB_DEVICE: cap::CapPtr = 92;
const USB_BAR: cap::CapPtr = 93;
const USB_IRQ: cap::CapPtr = 94;
const USB_MEMORY: cap::CapPtr = 95;

/// Find an xHCI controller and start its driver, which passes what its
/// keyboards and pointers say to `events`.
pub fn start(events: cap::CapPtr) {
    let Some((index, found)) = pci::find(|device| {
        device.header.class == xhci::CLASS
            && device.header.subclass == xhci::SUBCLASS
            && device.header.prog_if == xhci::PROG_IF
    }) else {
        println!("usb        : no xHCI controller");
        return;
    };
    let Some(region) = pci::region_of(index, 0) else {
        println!("usb        : the controller at {} has no registers", found.address);
        return;
    };
    let Some(driver) = modules::find("xhci")
        .and_then(|module| task::spawn_program_on("xhci", module.bytes, crate::cpu::index()).ok())
    else {
        println!("usb        : a controller at {}, but no driver for it", found.address);
        return;
    };
    // Its interrupt is aimed at the CPU the driver runs on.
    let line = task::cpu_of(driver).and_then(|cpu| irq::create(index, 0, cpu));
    let Some(line) = line else {
        println!("usb        : no interrupt for the controller at {}", found.address);
        return;
    };
    let Some(pages) = memory::create(xhci::MEMORY_PAGES, false) else {
        println!("usb        : no memory for the controller's rings");
        return;
    };

    let given = cap::mint(0, USB_DEVICE, cap::Object::PciDevice(index), Rights::ALL).is_ok()
        && cap::mint(0, USB_BAR, cap::Object::DeviceMemory(region), Rights::ALL).is_ok()
        && cap::mint(0, USB_IRQ, cap::Object::Interrupt(line), Rights::ALL).is_ok()
        && cap::mint(0, USB_MEMORY, cap::Object::Memory(memory::hold(pages)), Rights::ALL).is_ok()
        && cap::grant(driver, USB_DEVICE, xhci::DEVICE, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, USB_BAR, xhci::BAR, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, USB_IRQ, xhci::IRQ, Rights::READ).is_ok()
        && cap::grant(driver, USB_MEMORY, xhci::MEMORY, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, events, xhci::EVENTS, Rights::WRITE).is_ok()
        && cap::grant(driver, crate::LOG_ENDPOINT, xhci::LOG, Rights::WRITE).is_ok();
    crate::check(
        "usb",
        "the USB driver holds its controller, no more",
        given && cap::slots_used(driver) == Some(6),
    );
    if !given {
        return;
    }
    crate::start_all(&[Some(driver)]);
    println!("usb        : an xHCI controller at {}, driven in ring 3", found.address);
}
