//! 8253/8254 programmable interval timer, channel 0.
//!
//! Gives us a periodic tick on IRQ0. Crude and slow to reprogram, but it needs
//! no calibration and no ACPI tables, which makes it the right first clock. A
//! scheduler will want the APIC timer or TSC deadline instead.

use core::sync::atomic::{AtomicU64, Ordering};

use super::port::{inb, outb};

/// The PIT's fixed input frequency, ~1.193182 MHz.
const BASE_FREQUENCY: u32 = 1_193_182;

const CHANNEL0_DATA: u16 = 0x40;
const COMMAND: u16 = 0x43;

/// Channel 0, lobyte/hibyte access, mode 3 (square wave), binary.
const MODE_SQUARE_WAVE: u8 = 0x36;

/// IRQ line the PIT is wired to.
pub const IRQ: u8 = 0;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program channel 0 to fire at roughly `hz`.
///
/// The divisor is 16-bit, so the achievable range is about 19 Hz to 1.19 MHz;
/// the actual rate is reported back because the division rarely comes out even.
pub fn init(hz: u32) -> u32 {
    let divisor = (BASE_FREQUENCY / hz).clamp(1, u16::MAX as u32);
    let actual = BASE_FREQUENCY / divisor;

    // SAFETY: fixed legacy PIT ports. Writing the mode byte then the two
    // divisor halves in order is the documented programming sequence; no other
    // device is affected.
    unsafe {
        outb(COMMAND, MODE_SQUARE_WAVE);
        outb(CHANNEL0_DATA, (divisor & 0xff) as u8);
        outb(CHANNEL0_DATA, ((divisor >> 8) & 0xff) as u8);
    }

    actual
}

/// Called from the IRQ0 handler.
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Ticks since the timer was started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Channel 2: polled delays
// ---------------------------------------------------------------------------
//
// Channel 2 is the one wired to the PC speaker, and uniquely its gate is
// software-controlled (port 0x61 bit 0) and its output is readable (bit 5).
// That makes it the standard way to time a precise interval *without*
// interrupts -- which is exactly what calibrating the local APIC timer needs,
// since at that point the APIC isn't delivering anything yet.

const CHANNEL2_DATA: u16 = 0x42;
const PORT_61: u16 = 0x61;

/// Largest interval a 16-bit divisor can express, ~54.9 ms. Longer delays are
/// split into several passes.
const MAX_DELAY_US: u32 = 50_000;

/// Busy-wait for `us` microseconds using PIT channel 2.
///
/// Accurate to roughly a microsecond and completely independent of interrupts.
pub fn delay_us(us: u32) {
    let mut remaining = us;
    while remaining > 0 {
        let chunk = remaining.min(MAX_DELAY_US);
        delay_chunk(chunk);
        remaining -= chunk;
    }
}

fn delay_chunk(us: u32) {
    let divisor = ((BASE_FREQUENCY as u64 * us as u64) / 1_000_000) as u16;
    if divisor == 0 {
        return;
    }

    // SAFETY: ports 0x42, 0x43 and 0x61 are the fixed legacy PIT and system
    // control ports. We clear bit 1 of 0x61 so the speaker stays silent, and
    // only drive channel 2, which nothing else in the kernel uses.
    unsafe {
        // Gate high (bit 0 set) so the channel counts, speaker disconnected
        // (bit 1 clear) so none of this is audible.
        let port = (inb(PORT_61) & 0xfc) | 0x01;
        outb(PORT_61, port);

        // Channel 2, lobyte/hibyte, MODE 0 (interrupt on terminal count).
        //
        // Mode 0 specifically, because its output goes LOW the moment the mode
        // byte is written and only returns HIGH at terminal count -- which is
        // the edge the poll below waits for. Mode 1 looks like the natural
        // choice for a one-shot, but its output starts HIGH and only drops on
        // the gate edge, so polling for HIGH returns immediately and every
        // delay is a no-op.
        outb(COMMAND, 0xb0);
        outb(CHANNEL2_DATA, (divisor & 0xff) as u8);
        outb(CHANNEL2_DATA, (divisor >> 8) as u8);

        // Bit 5 mirrors the channel's output pin.
        while inb(PORT_61) & 0x20 == 0 {
            core::hint::spin_loop();
        }

        // Leave the gate as we found it.
        outb(PORT_61, port & 0xfe);
    }
}
