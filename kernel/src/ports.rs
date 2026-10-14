//! I/O ports, on behalf of whoever holds a capability to them.
//!
//! A few devices that matter -- the PS/2 controller above all -- are reached
//! through port instructions rather than memory. Ring 3 cannot execute those:
//! IOPL is 0, and giving a task an I/O permission bitmap would mean a TSS per
//! task or swapping 8 KiB on every switch. So a port is read or written by the
//! kernel, for a task that holds an [`Object::IoPorts`] capability covering
//! it, and the capability's range is the whole of what it may touch.
//!
//! A system call per access is slow, and that is fine for what is behind
//! ports today: a keyboard sends a byte when a key moves.
//!
//! [`Object::IoPorts`]: crate::cap::Object::IoPorts

use crate::arch::x86_64::port;
use crate::cap::{self, CapError, CapPtr, Rights};

/// The port `offset` bytes into the range in `slot`, if an access `width`
/// bytes wide starting there stays inside it.
///
/// `offset` and `width` are numbers from ring 3. The sum is done wide, so no
/// value of either can wrap past the end of the range and come back inside it.
fn port_at(slot: CapPtr, offset: u64, width: u64, required: Rights) -> Result<u16, CapError> {
    let (base, count) = cap::ports_of(slot, required)?;
    if !matches!(width, 1 | 2 | 4) {
        return Err(CapError::Invalid);
    }
    let end = offset.checked_add(width).ok_or(CapError::Invalid)?;
    if end > count as u64 {
        return Err(CapError::Invalid);
    }
    Ok(base + offset as u16)
}

/// Read `width` bytes at `offset` into the ports in `slot`. Requires READ.
pub fn read(slot: CapPtr, offset: u64, width: u64) -> Result<u32, CapError> {
    let port = port_at(slot, offset, width, Rights::READ)?;
    // SAFETY: the holder was granted this port. Reading a port changes
    // nothing in memory; what it does to the device is the device's business
    // and the holder's, which is what the capability is.
    unsafe {
        Ok(match width {
            1 => port::inb(port) as u32,
            2 => port::inw(port) as u32,
            _ => port::inl(port),
        })
    }
}

/// Write `value`, `width` bytes of it, at `offset` into the ports in `slot`.
/// Requires WRITE.
pub fn write(slot: CapPtr, offset: u64, width: u64, value: u64) -> Result<(), CapError> {
    let port = port_at(slot, offset, width, Rights::WRITE)?;
    // SAFETY: as for `read`. No port inside a range the kernel grants
    // reaches memory or interrupt routing: the kernel only grants ranges
    // belonging to the device it means to hand over.
    unsafe {
        match width {
            1 => port::outb(port, value as u8),
            2 => port::outw(port, value as u16),
            _ => port::outl(port, value as u32),
        }
    }
    Ok(())
}
