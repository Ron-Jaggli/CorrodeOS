//! I/O APIC.
//!
//! Takes device interrupt lines and routes each one to a vector on a chosen
//! local APIC. This is what replaces the 8259's fixed wiring: instead of IRQ n
//! meaning vector base+n forever, every line gets its own redirection entry
//! saying which vector, which CPU, and how the line is triggered.
//!
//! Register access is indirect and stateful -- write a register number to
//! IOREGSEL, then read or write IOWIN -- which means the pair is not reentrant.
//! An interrupt handler that touched the I/O APIC mid-sequence would corrupt
//! whatever was in flight, so all programming happens at init with interrupts
//! still disabled.

use crate::mm::paging;

/// Offsets from the controller's base address.
const IOREGSEL: usize = 0x00;
const IOWIN: usize = 0x10;

/// Register numbers.
const REG_ID: u32 = 0x00;
const REG_VERSION: u32 = 0x01;
/// First redirection entry; each one occupies two consecutive registers.
const REG_REDIRECT_BASE: u32 = 0x10;

/// Redirection entry flag bits (low word).
const ENTRY_ACTIVE_LOW: u32 = 1 << 13;
const ENTRY_LEVEL_TRIGGERED: u32 = 1 << 15;
const ENTRY_MASKED: u32 = 1 << 16;

/// Where I/O APIC registers get mapped.
const MMIO_VIRT: u64 = 0xffff_b000_0001_0000;

pub struct IoApic {
    base: *mut u32,
    gsi_base: u32,
    entries: u32,
}

impl IoApic {
    /// Map a controller's registers.
    pub fn new(physical_base: u64, gsi_base: u32) -> Option<Self> {
        let page = physical_base & !0xfff;
        let mapped = paging::with_kernel_mapper(|mapper| mapper.map_mmio(MMIO_VIRT, page));
        if mapped != Some(true) {
            return None;
        }

        let mut io = Self {
            base: (MMIO_VIRT | (physical_base & 0xfff)) as *mut u32,
            gsi_base,
            entries: 0,
        };

        // Bits 16-23 of the version register hold the highest entry index, so
        // the count is one more than that.
        // SAFETY: the registers are mapped.
        io.entries = ((unsafe { io.read(REG_VERSION) } >> 16) & 0xff) + 1;
        Some(io)
    }

    /// # Safety
    /// Registers must be mapped, and no interrupt handler may touch this
    /// controller concurrently -- the select/window pair is stateful.
    unsafe fn read(&self, register: u32) -> u32 {
        // SAFETY: caller guarantees exclusive access to the register pair.
        unsafe {
            self.base.byte_add(IOREGSEL).write_volatile(register);
            self.base.byte_add(IOWIN).read_volatile()
        }
    }

    /// # Safety
    /// As `read`, and `value` must be legal for `register`.
    unsafe fn write(&self, register: u32, value: u32) {
        // SAFETY: caller guarantees exclusive access to the register pair.
        unsafe {
            self.base.byte_add(IOREGSEL).write_volatile(register);
            self.base.byte_add(IOWIN).write_volatile(value);
        }
    }

    pub fn id(&self) -> u32 {
        // SAFETY: called at init, interrupts disabled.
        (unsafe { self.read(REG_ID) } >> 24) & 0xf
    }

    pub fn entry_count(&self) -> u32 {
        self.entries
    }

    pub fn gsi_base(&self) -> u32 {
        self.gsi_base
    }

    /// Mask every line this controller owns.
    ///
    /// Firmware does not guarantee a quiet controller on handover, and an
    /// unmasked line pointing at a vector we have not set up yet is an
    /// immediate unhandled interrupt.
    pub fn mask_all(&self) {
        for index in 0..self.entries {
            let register = REG_REDIRECT_BASE + index * 2;
            // SAFETY: called at init with interrupts disabled, so nothing else
            // is using the select/window pair.
            unsafe {
                self.write(register, ENTRY_MASKED);
                self.write(register + 1, 0);
            }
        }
    }

    /// Read back a redirection entry as `(low, high)`.
    ///
    /// Diagnostics: what the controller actually holds, as opposed to what we
    /// believe we wrote.
    pub fn entry(&self, gsi: u32) -> Option<(u32, u32)> {
        if gsi < self.gsi_base || gsi >= self.gsi_base + self.entries {
            return None;
        }
        let register = REG_REDIRECT_BASE + (gsi - self.gsi_base) * 2;
        // SAFETY: called at init with interrupts disabled.
        unsafe { Some((self.read(register), self.read(register + 1))) }
    }

    /// Route one global system interrupt to `vector` on `lapic_id`.
    ///
    /// `active_low` and `level` come from the MADT's interrupt source
    /// overrides; guessing them gets you an interrupt that fires once and never
    /// again, or one that never stops.
    pub fn route(
        &self,
        gsi: u32,
        vector: u8,
        lapic_id: u32,
        active_low: bool,
        level: bool,
    ) -> bool {
        if gsi < self.gsi_base || gsi >= self.gsi_base + self.entries {
            return false;
        }

        let index = gsi - self.gsi_base;
        let register = REG_REDIRECT_BASE + index * 2;

        let mut low = vector as u32;
        if active_low {
            low |= ENTRY_ACTIVE_LOW;
        }
        if level {
            low |= ENTRY_LEVEL_TRIGGERED;
        }

        // SAFETY: called at init with interrupts disabled. The high word
        // (destination) is written first so the entry is never briefly live
        // pointing at the wrong CPU: the low word's unmasked state is what
        // arms it.
        unsafe {
            self.write(register + 1, lapic_id << 24);
            self.write(register, low);
        }

        true
    }
}
