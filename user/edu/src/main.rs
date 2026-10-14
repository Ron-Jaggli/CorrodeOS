//! A driver for QEMU's `edu` device.
//!
//! The device does nothing useful. It exists to be driven: one register
//! answers with what was written to it inverted, one raises an interrupt
//! when told to, one works out a factorial and interrupts when it has. That
//! makes this the smallest program that is honestly a driver -- it reads
//! its device's identity, turns it on, maps its registers, and sleeps until
//! the device has something to say.
//!
//! It is an ordinary ring 3 program. It holds five capabilities, listed in
//! `corrode_abi::edu`: the device, its BAR, its interrupt, an endpoint to
//! report on and one to log to. It could not find another device if it
//! tried, because there is no call that would tell it one exists.
//!
//! Register offsets are from QEMU's docs/specs/edu.rst.

#![no_std]
#![no_main]

use corrode_rt::abi::edu::{
    BAR, DEVICE, FACTORIAL, INPUT, IRQ, LABEL_CONFIG, LABEL_MEMORY, LABEL_RAISED, LABEL_WORKED,
    LOG, PROBE, REPORT,
};
use corrode_rt::device::{Config, Interrupt, Mmio};
use corrode_rt::pci::{ConfigSpace, Width, BAR0, VENDOR_ID};
use corrode_rt::{println, send, status, Message};

corrode_rt::entry!(main);

/// Identification: 0xRRrr00ed, major and minor version then "ed".
const REG_IDENT: usize = 0x00;
/// Liveness: reads back the last value written, inverted.
const REG_LIVENESS: usize = 0x04;
/// Write a number, and later read its factorial.
const REG_FACTORIAL: usize = 0x08;
/// Bit 7: raise an interrupt when a factorial is done.
const REG_STATUS: usize = 0x20;
const STATUS_INTERRUPT_WHEN_DONE: u32 = 0x80;
/// Which interrupts are being asked for.
const REG_IRQ_STATUS: usize = 0x24;
/// Write a value to raise an interrupt with it.
const REG_IRQ_RAISE: usize = 0x60;
/// Write a value to stop asking.
const REG_IRQ_ACK: usize = 0x64;

fn report(label: u64, words: [u64; 4]) {
    // If the report cannot be made there is nobody to tell.
    let _ = send(REPORT, Message::new(label, words));
}

fn main() {
    corrode_rt::log::set(LOG);
    let config = Config(DEVICE);

    // Who am I driving?
    let identity = config.read32(VENDOR_ID);
    // Try to move the device. The BARs are the kernel's, so this must be
    // refused -- and the refusal is the thing worth reporting.
    let moved = config.try_write(BAR0, Width::Dword, 0xffff_ffff);
    let enabled = config.enable();
    report(
        LABEL_CONFIG,
        [identity as u64, status(&moved), status(&enabled), 0],
    );

    let registers = match Mmio::map(BAR) {
        Ok(registers) => registers,
        Err(error) => {
            println!("no registers: {:?}", error);
            return;
        }
    };

    let ident = registers.read32(REG_IDENT);
    registers.write32(REG_LIVENESS, PROBE);
    let answer = registers.read32(REG_LIVENESS);
    report(
        LABEL_MEMORY,
        [
            ident as u64,
            answer as u64,
            registers.len() as u64,
            registers.address() as u64,
        ],
    );

    // Tell the device to interrupt, and wait for it to.
    let irq = Interrupt(IRQ);
    registers.write32(REG_IRQ_RAISE, 1);
    let raised = irq.wait();
    let asking = registers.read32(REG_IRQ_STATUS);
    // Acknowledge, or the device believes it is still asking.
    registers.write32(REG_IRQ_ACK, asking);
    report(
        LABEL_RAISED,
        [
            status(&raised),
            raised.map_or(0, |fired| fired.count),
            asking as u64,
            raised.map_or(0, |fired| fired.from),
        ],
    );

    // Now some work, with an interrupt when it is done. This task is asleep
    // in the kernel while the device computes; nothing polls.
    registers.write32(REG_STATUS, STATUS_INTERRUPT_WHEN_DONE);
    registers.write32(REG_FACTORIAL, INPUT);
    let done = irq.wait();
    let factorial = registers.read32(REG_FACTORIAL);
    let asking = registers.read32(REG_IRQ_STATUS);
    registers.write32(REG_IRQ_ACK, asking);
    report(
        LABEL_WORKED,
        [
            status(&done),
            done.map_or(0, |fired| fired.count),
            factorial as u64,
            asking as u64,
        ],
    );

    println!(
        "{}! is {}, says the device ({})",
        INPUT,
        factorial,
        if factorial == FACTORIAL {
            "and it is right"
        } else {
            "and it is wrong"
        }
    );
}
