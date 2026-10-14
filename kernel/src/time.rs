//! Monotonic time, in nanoseconds since boot.
//!
//! The scheduling tick is a 10 ms grid, which is fine for deciding whose turn
//! it is and useless for motion: an animation sampled on that grid is quantised
//! to it, and one sampled *between* ticks cannot tell how far between. The TSC
//! answers "what time is it" to the nanosecond, from any context, by reading a
//! register -- no interrupt, no lock, no MMIO, and nothing that has to be kept
//! running to keep it true.
//!
//! Its rate is not documented anywhere the CPU will tell us reliably, so it is
//! measured once at boot against PIT channel 2, the same polled reference the
//! local APIC timer is calibrated against.
//!
//! # Across CPUs
//!
//! Each core has its own TSC. Firmware starts them in step and an invariant TSC
//! keeps them there, but nothing here depends on that: tasks are pinned, so a
//! task only ever compares readings taken on one core.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::x86_64::{lapic, pit};

/// TSC value treated as time zero.
static BASE: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds per TSC cycle, as a 32.32 fixed-point multiplier. Zero until
/// calibrated, which is what selects the tick fallback.
static NS_PER_CYCLE: AtomicU64 = AtomicU64::new(0);

/// What calibration found.
pub struct Clock {
    pub hz: u64,
    /// The CPU advertises a TSC that keeps one rate across frequency and
    /// sleep states. Without it the clock is still right while the core runs
    /// at the speed it was measured at, which is all this kernel does yet.
    pub invariant: bool,
}

fn rdtsc() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: `rdtsc` only writes eax and edx. Every x86_64 CPU has it, and
    // ring 0 may execute it whatever CR4.TSD says.
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | low as u64
}

/// `cpuid`, returning `(eax, edx)`.
fn cpuid(leaf: u32) -> (u32, u32) {
    let (eax, edx): (u32, u32);
    // SAFETY: `cpuid` exists on every x86_64 CPU and only writes the four
    // registers named here. It clobbers rbx, which the compiler reserves, so
    // that one is saved and restored by hand around it.
    unsafe {
        core::arch::asm!(
            "mov {saved}, rbx",
            "cpuid",
            "mov rbx, {saved}",
            saved = out(reg) _,
            inout("eax") leaf => eax,
            inout("ecx") 0u32 => _,
            out("edx") edx,
            options(nomem, nostack),
        );
    }
    (eax, edx)
}

fn invariant_tsc() -> bool {
    const POWER_MANAGEMENT: u32 = 0x8000_0007;
    let (highest, _) = cpuid(0x8000_0000);
    highest >= POWER_MANAGEMENT && cpuid(POWER_MANAGEMENT).1 & (1 << 8) != 0
}

/// Measure the TSC and start the clock at zero.
///
/// Call with interrupts off: a handler running inside the window is counted by
/// the TSC but delays noticing that the PIT finished, which reads as a faster
/// clock than the real one.
pub fn init() -> Option<Clock> {
    /// One PIT pass. Long enough that the few-microsecond error in catching
    /// the terminal count is under a hundred parts per million.
    const WINDOW_US: u32 = 50_000;

    let before = rdtsc();
    pit::delay_us(WINDOW_US);
    let after = rdtsc();

    let hz = after.wrapping_sub(before) * (1_000_000 / WINDOW_US as u64);
    if hz == 0 {
        return None;
    }

    BASE.store(after, Ordering::Relaxed);
    NS_PER_CYCLE.store((1_000_000_000u64 << 32) / hz, Ordering::Relaxed);

    Some(Clock {
        hz,
        invariant: invariant_tsc(),
    })
}

/// Nanoseconds since [`init`].
///
/// Falls back to the scheduling tick if the TSC could not be calibrated --
/// coarse, but still monotonic, so callers need no second code path.
pub fn now_ns() -> u64 {
    let scale = NS_PER_CYCLE.load(Ordering::Relaxed);
    if scale == 0 {
        let ticks = if lapic::timer_hz() > 0 {
            lapic::ticks()
        } else {
            pit::ticks()
        };
        return ticks * (1_000_000_000 / crate::TICK_HZ as u64);
    }

    let cycles = rdtsc().wrapping_sub(BASE.load(Ordering::Relaxed));
    ((cycles as u128 * scale as u128) >> 32) as u64
}
