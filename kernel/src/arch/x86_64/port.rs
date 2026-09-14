//! Raw x86 port I/O.
//!
//! This is the bottom of the `unsafe` stack: two instructions, no abstraction.
//! Everything above it (serial, PIC, PIT) is expected to be safe code wrapping
//! a specific, documented port.

use core::arch::asm;

/// Write a byte to an I/O port.
///
/// # Safety
/// Port I/O talks directly to hardware. The caller must know what device lives
/// at `port` and that writing `value` to it is a valid operation at this point
/// in boot. Writing to an arbitrary port can wedge the machine.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out` with these operand constraints has no effect on the Rust
    // abstract machine; correctness of the port/value pair is the caller's
    // obligation, documented above.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
/// The caller must know that `port` is a readable register on a device that is
/// present; reads can have side effects (e.g. clearing status registers).
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: as above -- the instruction itself touches no Rust-visible
    // memory; device validity is the caller's obligation.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}
