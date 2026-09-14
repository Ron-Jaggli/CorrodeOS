//! The legacy 8259A pair.
//!
//! Not the endgame -- the APIC is -- but the PIC is what QEMU gives us for free
//! and it's enough to prove the interrupt path with a real device.
//!
//! Remapping is mandatory, not cosmetic. Out of reset the master PIC raises
//! vectors 0x08-0x0F, which collide exactly with the CPU's own exception
//! vectors: a timer tick would arrive as a double fault. Every OS moves them.

use super::port::{inb, outb};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xa0;
const SLAVE_DATA: u16 = 0xa1;

/// First vector the master PIC will raise after remapping.
pub const VECTOR_BASE: u8 = 0x20;

const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const ICW4_8086: u8 = 0x01;
const EOI: u8 = 0x20;

/// Short delay for older PICs that need time between writes. Writing to the
/// unused POST port 0x80 is the traditional way to burn a bus cycle.
fn io_wait() {
    // SAFETY: port 0x80 is the POST diagnostic port. Writing to it has no
    // effect on any device we care about and is the conventional I/O delay.
    unsafe { outb(0x80, 0) };
}

/// Remap both PICs to vectors 0x20-0x2F and mask every line.
///
/// Callers unmask what they actually want afterwards.
pub fn init() {
    // SAFETY: this is the documented 8259A initialisation sequence, written to
    // the fixed legacy PIC ports. Interrupts must still be disabled here, which
    // they are -- `kmain` doesn't run `sti` until after this.
    unsafe {
        // Start initialisation; both chips then expect three more data writes.
        outb(MASTER_CMD, ICW1_INIT | ICW1_ICW4);
        io_wait();
        outb(SLAVE_CMD, ICW1_INIT | ICW1_ICW4);
        io_wait();

        // ICW2: vector offsets.
        outb(MASTER_DATA, VECTOR_BASE);
        io_wait();
        outb(SLAVE_DATA, VECTOR_BASE + 8);
        io_wait();

        // ICW3: how the two chips are wired -- slave hangs off master IRQ2.
        outb(MASTER_DATA, 1 << 2);
        io_wait();
        outb(SLAVE_DATA, 2);
        io_wait();

        // ICW4: 8086 mode rather than the 8080 default.
        outb(MASTER_DATA, ICW4_8086);
        io_wait();
        outb(SLAVE_DATA, ICW4_8086);
        io_wait();

        // Mask everything; nothing is ready to be handled yet.
        outb(MASTER_DATA, 0xff);
        outb(SLAVE_DATA, 0xff);
    }
}

/// Unmask a single IRQ line (0-15).
pub fn unmask(irq: u8) {
    let (port, bit) = if irq < 8 {
        (MASTER_DATA, irq)
    } else {
        (SLAVE_DATA, irq - 8)
    };
    // SAFETY: reading and writing the PIC mask register is side-effect free
    // beyond the intended mask change.
    unsafe {
        let mask = inb(port) & !(1 << bit);
        outb(port, mask);
    }
}

/// Acknowledge an interrupt. Without this the PIC never raises that line again.
///
/// Lines 8-15 arrive via the slave, which needs its own EOI as well as the
/// master's -- forgetting the master EOI is the classic "keyboard works once"
/// bug.
pub fn end_of_interrupt(irq: u8) {
    // SAFETY: writing EOI to the PIC command ports only clears in-service bits.
    unsafe {
        if irq >= 8 {
            outb(SLAVE_CMD, EOI);
        }
        outb(MASTER_CMD, EOI);
    }
}

/// Mask every line on both PICs.
///
/// Required before switching to the APIC. The 8259s stay physically wired and
/// will keep asserting INTR if anything is unmasked; with the APIC also
/// delivering, you get the same device interrupting twice on two different
/// vectors. Remapping them first (which `init` did) still matters even after
/// this, because a spurious IRQ7 can arrive from a masked PIC.
pub fn disable() {
    // SAFETY: writing 0xff to the mask registers only masks interrupts.
    unsafe {
        outb(MASTER_DATA, 0xff);
        outb(SLAVE_DATA, 0xff);
    }
}
