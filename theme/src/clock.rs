//! The menu bar's clock: seconds since 1970 as "Wed 19 Nov 14:05", in the
//! theme's timezone and hour style.
//!
//! No allocation, so the kernel and programs alike can use it: the text is
//! written into a buffer the caller owns.

use crate::Theme;

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A day since 1970 as (year, month 1-12, day 1-31): Howard Hinnant's
/// `civil_from_days`, exact for every day this clock will see.
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + (month <= 2) as i64;
    (year, month, day)
}

/// Write `unix` seconds as the menu bar shows them into `buffer`, and
/// return the text.
pub fn format<'a>(unix: i64, theme: &Theme, buffer: &'a mut [u8; 32]) -> &'a str {
    let local = unix + theme.utc_offset as i64 * 60;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (_, month, day) = civil(days);
    let (hour, minute) = (seconds / 3600, seconds / 60 % 60);

    let mut writer = Writer { buffer, len: 0 };
    let weekday = DAYS[days.rem_euclid(7) as usize];
    let month = MONTHS[month as usize - 1];
    let _ = if theme.clock_24h {
        core::fmt::write(
            &mut writer,
            format_args!("{weekday} {day} {month}  {hour:02}:{minute:02}"),
        )
    } else {
        let (twelve, half) = match hour {
            0 => (12, "AM"),
            1..=11 => (hour, "AM"),
            12 => (12, "PM"),
            _ => (hour - 12, "PM"),
        };
        core::fmt::write(
            &mut writer,
            format_args!("{weekday} {day} {month}  {twelve}:{minute:02} {half}"),
        )
    };
    let len = writer.len;
    // Only ASCII is ever written.
    core::str::from_utf8(&buffer[..len]).unwrap_or("")
}

struct Writer<'a> {
    buffer: &'a mut [u8; 32],
    len: usize,
}

impl core::fmt::Write for Writer<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        let slot = self.buffer.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_become_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        // 2026-11-19.
        assert_eq!(civil(20_776), (2026, 11, 19));
    }

    #[test]
    fn the_clock_reads_as_macos_writes_it() {
        let mut buffer = [0; 32];
        // 2026-11-19 14:05:00 UTC, a Thursday.
        let unix = 20_776 * 86_400 + 14 * 3600 + 5 * 60;
        let utc = crate::CORRODE;
        assert_eq!(format(unix, &utc, &mut buffer), "Thu 19 Nov  14:05");
        let twelve = Theme {
            clock_24h: false,
            utc_offset: -8 * 60,
            ..utc
        };
        assert_eq!(format(unix, &twelve, &mut buffer), "Thu 19 Nov  6:05 AM");
        let east = Theme {
            utc_offset: 11 * 60,
            ..utc
        };
        assert_eq!(format(unix, &east, &mut buffer), "Fri 20 Nov  01:05");
    }
}
