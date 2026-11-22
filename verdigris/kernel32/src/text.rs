//! The C runtime's pure parts: splitting a command line and reading a
//! number. No pointers, no state, no Windows calls, so the host tests
//! (`verdigris/crt-tests`) build this file as it is and check it against
//! the values the Microsoft runtime gives.

use alloc::vec::Vec;

/// A command line split into arguments as the Microsoft C runtime splits
/// it: white space between arguments; quotes group and are removed; `2n`
/// backslashes before a quote are `n` backslashes and the quote counts,
/// `2n+1` are `n` and a literal quote; `""` inside quotes is a literal
/// quote; other backslashes are themselves. The first argument, the
/// program's name, takes quotes but not backslash rules.
pub fn split(line: &[u8]) -> Vec<Vec<u8>> {
    let mut arguments = Vec::new();
    let mut at = 0;
    // The program's name.
    let mut name = Vec::new();
    let mut quoted = false;
    while at < line.len() {
        let byte = line[at];
        if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && (byte == b' ' || byte == b'\t') {
            break;
        } else {
            name.push(byte);
        }
        at += 1;
    }
    arguments.push(name);
    loop {
        while at < line.len() && (line[at] == b' ' || line[at] == b'\t') {
            at += 1;
        }
        if at == line.len() {
            break;
        }
        let mut argument = Vec::new();
        let mut quoted = false;
        while at < line.len() {
            let byte = line[at];
            if !quoted && (byte == b' ' || byte == b'\t') {
                break;
            }
            if byte == b'\\' {
                let run = line[at..].iter().take_while(|byte| **byte == b'\\').count();
                at += run;
                if line.get(at) == Some(&b'"') {
                    argument.extend(core::iter::repeat_n(b'\\', run / 2));
                    if run % 2 == 1 {
                        argument.push(b'"');
                        at += 1;
                    }
                } else {
                    argument.extend(core::iter::repeat_n(b'\\', run));
                }
                continue;
            }
            if byte == b'"' {
                if quoted && line.get(at + 1) == Some(&b'"') {
                    argument.push(b'"');
                    at += 2;
                    continue;
                }
                quoted = !quoted;
                at += 1;
                continue;
            }
            argument.push(byte);
            at += 1;
        }
        arguments.push(argument);
    }
    arguments
}

/// A number as `strtol` and `strtoul` read it, from the start of `text`:
/// leading white space, an optional sign, then digits in `base` (2 to 36,
/// or 0 to take it from the prefix: `0x` hex, `0` octal, else decimal; base
/// 16 also allows `0x`).
#[derive(Debug, PartialEq, Eq)]
pub struct Number {
    pub negative: bool,
    /// The digits' value, or `None` if it passed `u64::MAX`.
    pub magnitude: Option<u64>,
    /// Bytes read, through the last digit; 0 when there was no number, so
    /// `endptr` stays at the start, as the C standard says.
    pub len: usize,
}

/// C's white space in the "C" locale.
pub fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn digit(byte: u8) -> Option<u64> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u64),
        b'a'..=b'z' => Some((byte - b'a') as u64 + 10),
        b'A'..=b'Z' => Some((byte - b'A') as u64 + 10),
        _ => None,
    }
}

/// Read a number from `text` (the bytes up to its zero). A base outside 0
/// and 2 to 36 reads nothing.
pub fn number(text: &[u8], base: u32) -> Number {
    let none = Number { negative: false, magnitude: Some(0), len: 0 };
    if base == 1 || base > 36 {
        return none;
    }
    let mut base = base as u64;
    let mut at = 0;
    while at < text.len() && is_space(text[at]) {
        at += 1;
    }
    let mut negative = false;
    if let Some(sign @ (b'+' | b'-')) = text.get(at) {
        negative = *sign == b'-';
        at += 1;
    }
    let hex_prefix = text.get(at) == Some(&b'0')
        && matches!(text.get(at + 1), Some(b'x' | b'X'))
        && text.get(at + 2).and_then(|b| digit(*b)).is_some_and(|d| d < 16);
    if (base == 0 || base == 16) && hex_prefix {
        base = 16;
        at += 2;
    } else if base == 0 {
        base = if text.get(at) == Some(&b'0') { 8 } else { 10 };
    }
    let start = at;
    let mut magnitude = Some(0u64);
    while let Some(value) = text.get(at).and_then(|b| digit(*b)).filter(|d| *d < base) {
        magnitude = magnitude.and_then(|m| m.checked_mul(base)).and_then(|m| m.checked_add(value));
        at += 1;
    }
    if at == start {
        return none;
    }
    Number { negative, magnitude, len: at }
}

/// `strtol`'s answer for a `long` of 32 bits, as on Windows: the value,
/// clamped to `LONG_MIN`/`LONG_MAX`, and whether it was (`ERANGE`).
pub fn to_long(number: &Number) -> (i32, bool) {
    match number.magnitude {
        Some(m) if !number.negative && m <= i32::MAX as u64 => (m as i32, false),
        Some(m) if number.negative && m <= 1 << 31 => ((m as i64).wrapping_neg() as i32, false),
        _ if number.negative => (i32::MIN, true),
        _ => (i32::MAX, true),
    }
}

/// `strtoul`'s answer for an `unsigned long` of 32 bits: out of range is
/// `ULONG_MAX` and `ERANGE`; a minus sign negates in unsigned arithmetic,
/// so "-1" is `ULONG_MAX` with no error.
pub fn to_unsigned_long(number: &Number) -> (u32, bool) {
    match number.magnitude {
        Some(m) if m <= u32::MAX as u64 => {
            let value = m as u32;
            (if number.negative { value.wrapping_neg() } else { value }, false)
        }
        _ => (u32::MAX, true),
    }
}
