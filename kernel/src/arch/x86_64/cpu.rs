//! Which CPU is this.
//!
//! Every per-CPU structure -- TSS, double-fault stack, idle task, the
//! scheduler's notion of what is running -- is an array indexed by the number
//! this module hands out. The number is dense and assigned at boot, which the
//! LAPIC ID is not: firmware is free to hand out sparse IDs, so using one
//! directly as an array index is a habit that works in QEMU and falls over on
//! real hardware.
//!
//! # How it is looked up
//!
//! By reading the LAPIC's own ID register and consulting a table built during
//! SMP bring-up. That is an uncached MMIO read on a path taken by every timer
//! tick, which is a real cost and the obvious thing to optimise later: parking a
//! pointer to a per-CPU structure in `IA32_GS_BASE` turns this into one `mov`
//! from `gs:[0]`. Not done yet because it is an optimisation to a mechanism
//! that has to be correct first, and correctness here is a lookup table.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::lapic;

/// CPUs we are prepared to run on.
///
/// A fixed bound, so every per-CPU array is a static and nothing about bringing
/// a CPU online needs the heap. Extra CPUs beyond this are left parked and
/// reported rather than silently ignored.
pub const MAX_CPUS: usize = 8;

/// No CPU has been assigned this LAPIC ID.
const UNASSIGNED: usize = usize::MAX;

/// LAPIC ID -> dense index. 256 entries because xAPIC IDs are 8 bits.
static BY_LAPIC_ID: [AtomicUsize; 256] = [const { AtomicUsize::new(UNASSIGNED) }; 256];

/// How many CPUs have been given an index.
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Give a LAPIC ID a dense index, or return the one it already has.
///
/// Called once per CPU during bring-up, from the BSP, before the CPU in
/// question is started -- so the table is complete before anything reads it.
pub fn register(lapic_id: u32) -> Option<usize> {
    let slot = &BY_LAPIC_ID[lapic_id as usize & 0xff];
    let existing = slot.load(Ordering::Relaxed);
    if existing != UNASSIGNED {
        return Some(existing);
    }

    let index = COUNT.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_CPUS {
        // Put the counter back so `count()` stays truthful about how many
        // CPUs are actually usable.
        COUNT.store(MAX_CPUS, Ordering::Relaxed);
        return None;
    }

    slot.store(index, Ordering::Relaxed);
    Some(index)
}

/// Index of the CPU this is running on.
///
/// Returns 0 before the LAPIC is up, which is correct rather than a fallback:
/// until APs are started there is exactly one CPU and it is the bootstrap
/// processor.
pub fn index() -> usize {
    if lapic::timer_hz() == 0 && !lapic::is_mapped() {
        return 0;
    }
    let assigned = BY_LAPIC_ID[lapic::id() as usize & 0xff].load(Ordering::Relaxed);
    if assigned == UNASSIGNED {
        0
    } else {
        assigned
    }
}
