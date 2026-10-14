//! What a driver holds: configuration space, registers, an interrupt.
//!
//! Three capabilities, three small types. None of them can be made from a
//! bus address or a physical one, because there is no system call that
//! takes either.

use core::marker::PhantomData;

use corrode_pci::{ConfigSpace, Width};

use crate::{abi, check, sys, Error, Result};

/// A PCI function's configuration space, through the capability in a slot.
///
/// Implements [`ConfigSpace`], so the decoding in `corrode-pci` -- the same
/// code the kernel used to find this device -- works on it unchanged.
#[derive(Clone, Copy)]
pub struct Config(pub usize);

impl Config {
    /// Read a register, or say why not.
    pub fn try_read(&self, offset: u16, width: Width) -> Result<u32> {
        let out = sys::call(
            abi::SYS_CONFIG_READ,
            sys::Args {
                rdi: self.0 as u64,
                rsi: offset as u64,
                rdx: width.bytes() as u64,
                ..sys::Args::NONE
            },
        );
        check(out.rax)?;
        Ok(out.rdx as u32)
    }

    /// Write a register, or say why not. The kernel keeps some for itself:
    /// those are [`Error::Invalid`] whatever rights this capability carries.
    pub fn try_write(&self, offset: u16, width: Width, value: u32) -> Result<()> {
        let out = sys::call(
            abi::SYS_CONFIG_WRITE,
            sys::Args {
                rdi: self.0 as u64,
                rsi: offset as u64,
                rdx: width.bytes() as u64,
                r10: value as u64,
                ..sys::Args::NONE
            },
        );
        check(out.rax)
    }

    /// Turn on memory decoding and bus mastering: what nearly every driver
    /// does first. Bus mastering is what lets the device raise a message
    /// signalled interrupt at all, since that is a write it makes itself.
    pub fn enable(&self) -> Result<()> {
        let command = self.try_read(corrode_pci::COMMAND, Width::Word)?;
        let wanted = corrode_pci::COMMAND_MEMORY | corrode_pci::COMMAND_BUS_MASTER;
        self.try_write(corrode_pci::COMMAND, Width::Word, command | wanted as u32)
    }
}

impl ConfigSpace for Config {
    /// A read that fails comes back as all ones, which is what the bus
    /// itself answers when nothing is there.
    fn read(&self, offset: u16, width: Width) -> u32 {
        self.try_read(offset, width).unwrap_or(0xffff_ffff)
    }

    /// A write that is refused is dropped. A caller that needs to know uses
    /// [`Config::try_write`].
    fn write(&self, offset: u16, width: Width, value: u32) {
        let _ = self.try_write(offset, width, value);
    }
}

/// A device's registers, mapped.
///
/// Every access is checked against the length the kernel reported and is
/// volatile. An offset past the end is a bug in the driver and panics; it
/// is not a way to reach the next device along.
pub struct Mmio {
    base: *mut u8,
    len: usize,
    /// Registers are shared with a device that changes them on its own, so
    /// this is not `Sync`: one task owns one device.
    _device: PhantomData<*mut u8>,
}

impl Mmio {
    /// Map the device memory in `slot`. The kernel says where it went.
    pub fn map(slot: usize) -> Result<Mmio> {
        let mapping = crate::map(slot)?;
        if mapping.address.is_null() || mapping.len == 0 {
            return Err(Error::Invalid);
        }
        Ok(Mmio {
            base: mapping.address,
            len: mapping.len,
            _device: PhantomData,
        })
    }

    pub fn address(&self) -> usize {
        self.base as usize
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn at<T>(&self, offset: usize) -> *mut T {
        let size = core::mem::size_of::<T>();
        assert!(
            offset.is_multiple_of(size)
                && offset.checked_add(size).is_some_and(|end| end <= self.len),
            "register access at {offset:#x} is outside the {:#x} bytes mapped",
            self.len
        );
        // SAFETY: just checked to be inside the mapping, so the offset
        // pointer stays within the one allocation the kernel gave us.
        unsafe { self.base.add(offset).cast() }
    }

    pub fn read8(&self, offset: usize) -> u8 {
        // SAFETY: `at` checked the bounds and alignment; the kernel mapped
        // these bytes readable and they stay mapped while this task lives.
        unsafe { self.at::<u8>(offset).read_volatile() }
    }

    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: as `read8`.
        unsafe { self.at::<u16>(offset).read_volatile() }
    }

    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: as `read8`.
        unsafe { self.at::<u32>(offset).read_volatile() }
    }

    pub fn read64(&self, offset: usize) -> u64 {
        // SAFETY: as `read8`.
        unsafe { self.at::<u64>(offset).read_volatile() }
    }

    pub fn write8(&self, offset: usize, value: u8) {
        // SAFETY: as `read8`. If the capability carried no WRITE the page is
        // read-only and this faults, which stops this task and nothing else.
        unsafe { self.at::<u8>(offset).write_volatile(value) }
    }

    pub fn write16(&self, offset: usize, value: u16) {
        // SAFETY: as `write8`.
        unsafe { self.at::<u16>(offset).write_volatile(value) }
    }

    pub fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as `write8`.
        unsafe { self.at::<u32>(offset).write_volatile(value) }
    }

    pub fn write64(&self, offset: usize, value: u64) {
        // SAFETY: as `write8`.
        unsafe { self.at::<u64>(offset).write_volatile(value) }
    }
}

/// A device's interrupt.
#[derive(Clone, Copy)]
pub struct Interrupt(pub usize);

/// What waiting for an interrupt produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fired {
    /// How many times the device has signalled since this was last asked.
    pub count: u64,
    /// Who the message came from: [`abi::FROM_HARDWARE`].
    pub from: u64,
}

impl Interrupt {
    /// Block until the device signals. If it already has, return at once.
    ///
    /// The same system call as receiving a message, because it is the same
    /// thing: waiting for whoever is on the other end to have something to
    /// say. Here that is the hardware, and all it says is how many times.
    pub fn wait(&self) -> Result<Fired> {
        let received = crate::recv(self.0, None, None)?;
        Ok(Fired {
            count: received.message.words[0],
            from: received.from,
        })
    }
}
