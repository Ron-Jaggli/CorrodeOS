//! Device interrupts, delivered to whoever holds the capability.
//!
//! A driver in ring 3 cannot take an interrupt. What it can do is wait for
//! one, and the thing it waits on is an object like any other: an
//! **interrupt** capability, received on with the same system call that
//! receives a message from an endpoint. The hardware is the sender. It has
//! nothing to say but "again", so the message is a count of how many times
//! the device has signalled since the holder last asked.
//!
//! # Message signalled only
//!
//! An MSI is the device writing a word to an address the local APIC listens
//! on. The word is the vector and the address picks the CPU, so there is no
//! shared line, no polarity and no trigger mode to get wrong, and nothing to
//! mask and unmask around the handler: each message is one edge. The kernel
//! writes the message into the device -- the MSI capability in its
//! configuration space, or an entry of the MSI-X table behind one of its
//! BARs -- and a driver can do neither, which is what stops it aiming an
//! interrupt at a vector that is not its own.
//!
//! # Nothing is lost and nothing is queued
//!
//! The handler adds one to a counter and, if a task is waiting, makes it
//! runnable. A message that arrives with nobody waiting is not dropped: the
//! next receive finds the count and returns at once. A thousand messages
//! with nobody waiting are one number, not a thousand of anything, so a
//! device that storms cannot spend kernel memory.
//!
//! # Which CPU
//!
//! A task is pinned to a CPU, so the message is aimed at the CPU of the task
//! it was made for. The handler then runs where the waiter does and can
//! switch to it on the way out, rather than leaving it to be found at that
//! core's next tick.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use corrode_pci::{MSIX_ENTRY_LEN, MSIX_ENTRY_MASKED};

use crate::arch::x86_64::{cpu, idt::TrapFrame, lapic, vectors, without_interrupts};
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::paging;
use crate::pci;
use crate::task::{self, State};

/// How many interrupt lines there are: one vector each.
pub const LINES: usize = (vectors::DEVICE_END - vectors::DEVICE_BASE) as usize;

/// Where MSI-X tables are mapped: one page per line, beside the APICs and
/// configuration space.
const MSIX_VIRT: u64 = 0xffff_b000_2000_0000;

/// Nobody is waiting.
const NOBODY: usize = usize::MAX;

struct Line {
    /// Messages since the holder last asked.
    pending: AtomicU64,
    /// Every message ever, for the boot checks.
    total: AtomicU64,
    /// The task blocked in [`wait`], or [`NOBODY`].
    waiter: AtomicUsize,
}

static TABLE: [Line; LINES] = [const {
    Line {
        pending: AtomicU64::new(0),
        total: AtomicU64::new(0),
        waiter: AtomicUsize::new(NOBODY),
    }
}; LINES];

/// Lines handed out so far. They are never handed back.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Make an interrupt line for one message of one device, aimed at the CPU
/// with index `cpu_index`, and return its number.
///
/// `entry` picks a row of the MSI-X table; a device with plain MSI has one
/// message and `entry` must be zero. `None` if the device has neither kind,
/// the local APIC is not in use, or the lines have run out.
///
/// This creates an object and no authority over it: that is a capability,
/// and minting one is the caller's decision. The device will not actually
/// signal until its driver sets the bus-master bit, because a message is a
/// write the device makes on its own.
pub fn create(device: usize, entry: u16, cpu_index: usize) -> Option<usize> {
    if !lapic::is_mapped() {
        return None;
    }
    let found = pci::device(device)?;
    let apic_id = cpu::lapic_id_of(cpu_index)?;
    if found.msix.is_none() && (found.msi.is_none() || entry != 0) {
        return None;
    }

    let index = NEXT.fetch_add(1, Ordering::Relaxed);
    if index >= LINES {
        NEXT.store(LINES, Ordering::Relaxed);
        return None;
    }
    let vector = vectors::DEVICE_BASE + index as u8;
    let (address, data) = corrode_pci::x86_message(apic_id, vector);
    let config = found.config();

    if let Some(msix) = found.msix {
        if entry >= msix.entries {
            return None;
        }
        let region = pci::region(pci::region_of(device, msix.table_bar)?)?;
        let at = region.physical + msix.table_offset as u64 + entry as u64 * MSIX_ENTRY_LEN;
        let va = MSIX_VIRT + index as u64 * 4096;
        let mapped = paging::with_kernel_mapper(|mapper| mapper.map_mmio(va, at & !0xfff));
        if mapped != Some(true) {
            return None;
        }
        let row = (va + (at & 0xfff)) as *mut u32;
        // SAFETY: the page holding this row of the device's MSI-X table was
        // just mapped uncached at `va`. A row is sixteen bytes on a sixteen
        // byte boundary, so all four words are inside the page. No task can
        // reach these bytes: `device::map` leaves the table's pages out.
        unsafe {
            // Masked while the message is half written.
            row.add(3).write_volatile(MSIX_ENTRY_MASKED);
            row.write_volatile(address as u32);
            row.add(1).write_volatile((address >> 32) as u32);
            row.add(2).write_volatile(data as u32);
            row.add(3).write_volatile(0);
        }
        msix.enable(&config);
    } else if let Some(msi) = found.msi {
        msi.enable(&config, address, data);
    }

    Some(index)
}

/// A device signalled. Called from the trap path, interrupts off.
///
/// Returns the frame to resume: the waiter's CPU reschedules on the way out,
/// so a driver woken here runs now rather than at the next tick.
pub fn on_interrupt(vector: u8, frame: *mut TrapFrame) -> *mut TrapFrame {
    let line = &TABLE[(vector - vectors::DEVICE_BASE) as usize];
    // Counted before the waiter is looked at. `wait` writes itself down as
    // the waiter and then looks at the count, so whichever order the two
    // run in, one of them sees the other.
    line.pending.fetch_add(1, Ordering::SeqCst);
    line.total.fetch_add(1, Ordering::Relaxed);
    lapic::end_of_interrupt();

    let waiter = line.waiter.load(Ordering::SeqCst);
    if waiter == NOBODY {
        return frame;
    }
    let here = task::with_task(waiter, |task| {
        if task.state == State::Blocked {
            task.state = State::Ready;
        }
        task.cpu == cpu::index()
    });
    if here == Some(true) {
        task::on_tick(frame)
    } else {
        frame
    }
}

/// Block until the interrupt in `slot` has fired, and return how many times
/// it has since this was last asked. Requires READ.
///
/// One waiter at a time: a second task waiting on the same line while the
/// first is parked is [`CapError::Refused`], rather than one of them being
/// silently forgotten.
pub fn wait(slot: CapPtr) -> Result<u64, CapError> {
    let index = cap::interrupt_of(slot, Rights::READ)?;
    let line = TABLE.get(index).ok_or(CapError::Stale)?;
    let me = task::current_id().ok_or(CapError::NoSuchTask)?;

    loop {
        let fired = without_interrupts(|| {
            let count = line.pending.swap(0, Ordering::SeqCst);
            if count > 0 {
                let _ =
                    line.waiter
                        .compare_exchange(me, NOBODY, Ordering::SeqCst, Ordering::SeqCst);
                return Ok(Some(count));
            }

            match line
                .waiter
                .compare_exchange(NOBODY, me, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {}
                Err(already) if already == me => {}
                Err(_) => return Err(CapError::Refused),
            }
            task::with_task(me, |task| task.state = State::Blocked);
            // Looked at again now that we are written down: a message that
            // landed between the first look and here woke nobody.
            if line.pending.load(Ordering::SeqCst) > 0 {
                task::with_task(me, |task| task.state = State::Running);
            }
            Ok(None)
        })?;

        match fired {
            Some(count) => return Ok(count),
            None => task::yield_now(),
        }
    }
}

/// How many times a line has fired, ever.
pub fn total(index: usize) -> u64 {
    TABLE
        .get(index)
        .map_or(0, |line| line.total.load(Ordering::Relaxed))
}
