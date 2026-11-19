//! The wall clock: the CMOS real-time clock, read once at boot.
//!
//! Every PC has one, behind ports 0x70 and 0x71, keeping the date while the
//! machine is off. It is read once, paired with the monotonic clock at that
//! instant, and the wall time after that is the pair plus how far the
//! monotonic clock has run. Nothing re-reads the slow chip.
//!
//! QEMU sets it to the host's UTC. On real hardware it may be local time;
//! `timezone` in `/etc/desktop.conf` is how a person says which.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::arch::x86_64::port::{inb, outb};

/// Seconds since 1970 at [`AT_NS`], or `i64::MIN` before it was read.
static UNIX: AtomicI64 = AtomicI64::new(i64::MIN);
static AT_NS: AtomicU64 = AtomicU64::new(0);

fn register(index: u8) -> u8 {
    // SAFETY: 0x70 selects a CMOS register and 0x71 reads it; both ports
    // belong to the RTC on every PC, and nothing else in the kernel touches
    // them. Bit 7 of the index is left clear, keeping NMIs enabled.
    unsafe {
        outb(0x70, index & 0x7f);
        inb(0x71)
    }
}

/// Every register the date needs, read in one go.
fn snapshot() -> [u8; 7] {
    [0x00, 0x02, 0x04, 0x07, 0x08, 0x09, 0x32].map(register)
}

/// Read the clock and remember it against the monotonic clock. Call once,
/// at boot.
pub fn init() {
    // Wait out an update in progress, then read until two readings agree:
    // the chip ticks over between reads otherwise.
    let mut spins = 0;
    while register(0x0a) & 0x80 != 0 && spins < 100_000 {
        spins += 1;
    }
    let mut reading = snapshot();
    for _ in 0..5 {
        let again = snapshot();
        if again == reading {
            break;
        }
        reading = again;
    }
    let status_b = register(0x0b);
    let bcd = status_b & 0x04 == 0;
    let decode = |value: u8| -> i64 {
        if bcd {
            ((value >> 4) * 10 + (value & 0x0f)) as i64
        } else {
            value as i64
        }
    };
    let [second, minute, hour_raw, day, month, year, century] = reading;
    // 12-hour mode keeps PM in the hour's top bit.
    let pm = status_b & 0x02 == 0 && hour_raw & 0x80 != 0;
    let mut hour = decode(hour_raw & 0x7f);
    if status_b & 0x02 == 0 {
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    let century = match decode(century) {
        19..=21 => decode(century),
        _ => 20,
    };
    let year = century * 100 + decode(year);
    let (month, day) = (decode(month), decode(day));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 2000 {
        crate::println!("rtc        : the clock reads nonsense -- no wall time");
        return;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let unix = days * 86_400 + hour * 3600 + decode(minute) * 60 + decode(second);
    AT_NS.store(crate::time::now_ns(), Ordering::Relaxed);
    UNIX.store(unix, Ordering::Release);
    crate::println!(
        "rtc        : {year}-{month:02}-{day:02} {hour:02}:{:02}:{:02} UTC",
        decode(minute),
        decode(second)
    );
}

/// Days since 1970 of a date: Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The wall clock as seconds since 1970 and the monotonic nanosecond it was
/// read at: enough for anyone to tell the time from then on.
pub fn anchor() -> Option<(i64, u64)> {
    let unix = UNIX.load(Ordering::Acquire);
    (unix != i64::MIN).then(|| (unix, AT_NS.load(Ordering::Relaxed)))
}
