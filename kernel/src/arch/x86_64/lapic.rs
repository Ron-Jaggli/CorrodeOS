//! Local APIC.
//!
//! One per CPU, and the thing that actually delivers interrupts to a core. It
//! replaces the 8259 for everything: the I/O APIC routes device interrupts to
//! it, and its built-in timer replaces the PIT as the scheduling tick.
//!
//! The timer has no fixed frequency -- it counts at the bus/core clock divided
//! by a programmable divisor, and that rate varies by machine. So it has to be
//! calibrated against something with a known rate, which is what the PIT's
//! channel 2 is still good for.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::paging;

use super::pit;

/// Register offsets, in bytes from the LAPIC base.
const REG_ID: usize = 0x020;
const REG_VERSION: usize = 0x030;
const REG_TPR: usize = 0x080;
const REG_EOI: usize = 0x0b0;
const REG_SPURIOUS: usize = 0x0f0;
const REG_ICR_LOW: usize = 0x300;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3e0;

/// Bit 8 of the spurious-interrupt register: APIC software enable.
const SPURIOUS_ENABLE: u32 = 1 << 8;

/// Bit 16 of an LVT entry masks it.
const LVT_MASKED: u32 = 1 << 16;
/// Bits 17-18 = 01: periodic mode.
const LVT_PERIODIC: u32 = 1 << 17;

/// Interrupt command: assert level, and target every CPU but this one.
const ICR_ASSERT: u32 = 1 << 14;
const ICR_ALL_EXCLUDING_SELF: u32 = 0b11 << 18;
/// Set while an interprocessor interrupt is still being delivered.
const ICR_SEND_PENDING: u32 = 1 << 12;

/// Divide configuration value for "divide by 16".
///
/// The encoding is genuinely strange: the divisor is built from bits 3, 1 and 0
/// with bit 2 skipped, so 0b0011 means 16 and 0b1011 means 1.
const DIVIDE_BY_16: u32 = 0b0011;

/// Where the LAPIC registers are mapped. Chosen to sit well clear of the heap
/// and the direct map.
const MMIO_VIRT: u64 = 0xffff_b000_0000_0000;

static BASE: AtomicU64 = AtomicU64::new(0);
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
static PER_CPU_TICKS: [AtomicU64; super::cpu::MAX_CPUS] =
    [const { AtomicU64::new(0) }; super::cpu::MAX_CPUS];

fn base() -> *mut u32 {
    BASE.load(Ordering::Relaxed) as *mut u32
}

/// # Safety
/// The LAPIC must be mapped and `offset` must be a valid register offset.
unsafe fn read(offset: usize) -> u32 {
    // SAFETY: caller guarantees the mapping exists. LAPIC registers are 32 bits
    // wide on 16-byte centres, and the mapping is uncached so the read reaches
    // the device rather than a stale cache line.
    unsafe { base().byte_add(offset).read_volatile() }
}

/// # Safety
/// The LAPIC must be mapped, `offset` must be a writable register, and `value`
/// must be legal for it.
unsafe fn write(offset: usize, value: u32) {
    // SAFETY: as above.
    unsafe { base().byte_add(offset).write_volatile(value) };
}

/// Map the LAPIC and turn it on.
///
/// Returns false if the registers could not be mapped.
pub fn init(physical_base: u64) -> bool {
    // The LAPIC lives in a 4 KiB page; keep any low bits out of the mapping.
    let page = physical_base & !0xfff;
    let mapped = paging::with_kernel_mapper(|mapper| mapper.map_mmio(MMIO_VIRT, page));
    if mapped != Some(true) {
        return false;
    }

    BASE.store(MMIO_VIRT | (physical_base & 0xfff), Ordering::Relaxed);

    // SAFETY: the registers are now mapped.
    unsafe {
        // Accept every interrupt priority class; a non-zero TPR here silently
        // blocks low-vector interrupts and is a miserable thing to debug.
        write(REG_TPR, 0);

        // Software-enable, and point spurious interrupts at a vector we have a
        // handler for.
        write(
            REG_SPURIOUS,
            SPURIOUS_ENABLE | super::vectors::SPURIOUS as u32,
        );
    }

    true
}

/// Whether the LAPIC registers have been mapped yet.
pub fn is_mapped() -> bool {
    BASE.load(Ordering::Relaxed) != 0
}

/// Software-enable *this* CPU's local APIC.
///
/// Separate from [`init`], which also maps the registers. The mapping is a
/// property of the address space and is shared; being enabled, having a
/// priority threshold and having a spurious vector are properties of each
/// CPU's own LAPIC and have to be set again on every one of them.
pub fn enable_local() {
    // SAFETY: the registers are mapped -- `init` ran on the bootstrap processor
    // before any AP was started, and the mapping lives in the kernel half of
    // every address space.
    unsafe {
        write(REG_TPR, 0);
        write(
            REG_SPURIOUS,
            SPURIOUS_ENABLE | super::vectors::SPURIOUS as u32,
        );
    }
}

pub fn id() -> u32 {
    // SAFETY: only called after a successful `init`.
    unsafe { read(REG_ID) >> 24 }
}

pub fn version() -> u32 {
    // SAFETY: only called after a successful `init`.
    unsafe { read(REG_VERSION) & 0xff }
}

/// Measure how fast the LAPIC timer counts, in ticks per second.
///
/// Counts down from a known value for a PIT-timed interval. The timer is left
/// masked throughout, so nothing is delivered while we measure.
pub fn calibrate() -> u64 {
    const SAMPLE_US: u32 = 20_000;

    // SAFETY: the LAPIC is mapped; all three registers are writable, and the
    // LVT entry is masked so no interrupt can be raised during the measurement.
    let elapsed = unsafe {
        write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
        write(REG_LVT_TIMER, LVT_MASKED);
        write(REG_TIMER_INITIAL, u32::MAX);

        pit::delay_us(SAMPLE_US);

        let remaining = read(REG_TIMER_CURRENT);
        write(REG_TIMER_INITIAL, 0);

        (u32::MAX - remaining) as u64
    };

    let hz = elapsed * (1_000_000 / SAMPLE_US as u64);
    TIMER_HZ.store(hz, Ordering::Relaxed);
    hz
}

/// Start the timer firing periodically at `hz` on `vector`.
///
/// Returns the count programmed, or 0 if calibration never ran.
pub fn start_timer(vector: u8, hz: u32) -> u32 {
    let rate = TIMER_HZ.load(Ordering::Relaxed);
    if rate == 0 || hz == 0 {
        return 0;
    }

    let count = (rate / hz as u64).clamp(1, u32::MAX as u64) as u32;

    // SAFETY: the LAPIC is mapped and calibration has run. Writing the initial
    // count last is deliberate: it is what actually starts the countdown, so
    // the LVT entry is already correct when the first tick lands.
    unsafe {
        write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
        write(REG_LVT_TIMER, LVT_PERIODIC | vector as u32);
        write(REG_TIMER_INITIAL, count);
    }

    count
}

/// Send `vector` to every CPU except this one.
///
/// Uses the "all excluding self" destination shorthand, so it needs no CPU list
/// and cannot be wrong about who exists -- the LAPIC knows.
///
/// A CPU with interrupts disabled will not take this until it enables them
/// again, which is the honest limit of a fixed IPI: it stops cores that are
/// running normally, not ones wedged inside a critical section. Stopping those
/// needs an NMI, which brings its own re-entrancy problems and is not worth it
/// for the one caller here.
pub fn broadcast(vector: u8) {
    if !is_mapped() {
        return;
    }
    // SAFETY: the LAPIC is mapped. Writing ICR low is what sends the IPI; the
    // spin afterwards waits for delivery to be accepted, so a second send
    // cannot overwrite one still in flight.
    unsafe {
        write(
            REG_ICR_LOW,
            vector as u32 | ICR_ASSERT | ICR_ALL_EXCLUDING_SELF,
        );
        let mut spins = 0u32;
        while read(REG_ICR_LOW) & ICR_SEND_PENDING != 0 && spins < 1_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }
    }
}

/// Signal end-of-interrupt.
///
/// Every interrupt delivered through the LAPIC needs this, including ones the
/// I/O APIC routed here. The one exception is the spurious vector, which must
/// *not* be acknowledged.
pub fn end_of_interrupt() {
    // SAFETY: only reached from interrupt handlers, which only run after init.
    unsafe { write(REG_EOI, 0) };
}

pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    PER_CPU_TICKS[super::cpu::index()].fetch_add(1, Ordering::Relaxed);
}

/// Ticks counted on one CPU.
///
/// The per-CPU split is what shows that every core's own timer is running.
/// A single total cannot: one core ticking at four times the rate looks
/// identical to four cores ticking correctly.
pub fn ticks_on(cpu: usize) -> u64 {
    PER_CPU_TICKS
        .get(cpu)
        .map(|t| t.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Wall-clock tick source: the bootstrap processor's own timer.
///
/// Deliberately *not* the total across CPUs. Every core ticks at the same rate,
/// so a sum advances N times too fast and any "seconds" derived from it means
/// nothing. One core's count is a clock; all of them added together is a
/// throughput measure wearing a clock's clothes.
pub fn ticks() -> u64 {
    ticks_on(0)
}

/// Ticks delivered across every CPU. A work total, not a clock.
pub fn ticks_total() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn timer_hz() -> u64 {
    TIMER_HZ.load(Ordering::Relaxed)
}
