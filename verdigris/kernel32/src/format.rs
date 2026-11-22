//! printf's formatting, for msvcrt's `printf` family.
//!
//! Arguments come as the Windows x64 calling convention leaves them for a
//! variadic function: one 8-byte slot each, in order, a `double` as its
//! bits, anything bigger than 8 bytes by address. A `va_list` is a pointer
//! to the next slot, so `vfprintf` and the varargs entries share this.
//!
//! What is done: flags `-+ #0`, width and precision (also `*`), lengths
//! `hh h l ll L z j t I I32 I64 w`, and conversions `d i u o x X c C s S p
//! e E f F g G a A %`. `%n` writes nothing and is skipped, as the Microsoft
//! runtime does by default. Floats are formatted by Rust's `core::fmt`.

use alloc::string::String;
use core::fmt::Write;

/// The arguments, one 8-byte slot at a time.
pub struct Args(pub *const u64);

impl Args {
    fn next(&mut self) -> u64 {
        // SAFETY: the caller of printf promised an argument for each
        // conversion; reading past them reads its own stack, as on Windows.
        let value = unsafe { self.0.read_unaligned() };
        // SAFETY: as above.
        self.0 = unsafe { self.0.add(1) };
        value
    }
}

#[derive(Default, Clone, Copy)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
    /// Bytes in an integer argument: 1, 2, 4 or 8.
    size: u8,
    wide: bool,
}

/// Format `format` (zero-ended) with `args`, giving each byte to `out`.
/// Returns how many bytes it gave.
///
/// # Safety
/// `format` is a zero-ended string; the `%s`-style arguments are pointers
/// to zero-ended strings (or null, shown as `(null)`).
pub unsafe fn format(format: *const u8, args: &mut Args, out: &mut dyn FnMut(u8)) -> usize {
    let mut count = 0usize;
    let mut emit = |byte: u8| {
        out(byte);
        count += 1;
    };
    let mut at = format;
    // SAFETY: the caller's promise: read only up to the zero.
    let peek = |at: *const u8| unsafe { at.read() };
    loop {
        let byte = peek(at);
        if byte == 0 {
            break;
        }
        // SAFETY: not past the zero.
        at = unsafe { at.add(1) };
        if byte != b'%' {
            emit(byte);
            continue;
        }
        let mut spec = Spec { size: 4, ..Spec::default() };
        loop {
            match peek(at) {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alternate = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            // SAFETY: the byte read was not the zero.
            at = unsafe { at.add(1) };
        }
        if peek(at) == b'*' {
            let width = args.next() as i32;
            if width < 0 {
                spec.left = true;
            }
            spec.width = width.unsigned_abs() as usize;
            // SAFETY: as above.
            at = unsafe { at.add(1) };
        } else {
            while peek(at).is_ascii_digit() {
                spec.width = spec.width.saturating_mul(10).saturating_add((peek(at) - b'0') as usize);
                // SAFETY: as above.
                at = unsafe { at.add(1) };
            }
        }
        if peek(at) == b'.' {
            // SAFETY: as above.
            at = unsafe { at.add(1) };
            if peek(at) == b'*' {
                let precision = args.next() as i32;
                spec.precision = (precision >= 0).then_some(precision as usize);
                // SAFETY: as above.
                at = unsafe { at.add(1) };
            } else {
                let mut precision = 0usize;
                while peek(at).is_ascii_digit() {
                    precision = precision.saturating_mul(10).saturating_add((peek(at) - b'0') as usize);
                    // SAFETY: as above.
                    at = unsafe { at.add(1) };
                }
                spec.precision = Some(precision);
            }
        }
        // Lengths. `l` is 4 bytes on Windows; `ll`, `I64`, `z`, `j`, `t`
        // and `I` are 8.
        loop {
            let take = |n: usize, at: &mut *const u8| {
                // SAFETY: the bytes matched were not the zero.
                *at = unsafe { at.add(n) }
            };
            match peek(at) {
                b'h' => {
                    spec.size = if spec.size == 2 { 1 } else { 2 };
                    take(1, &mut at);
                }
                b'l' => {
                    if spec.size == 4 && peek(unsafe { at.add(1) }) == b'l' {
                        spec.size = 8;
                        take(2, &mut at);
                    } else {
                        spec.wide = true;
                        take(1, &mut at);
                    }
                }
                b'w' => {
                    spec.wide = true;
                    take(1, &mut at);
                }
                b'L' => take(1, &mut at),
                b'z' | b'j' | b't' => {
                    spec.size = 8;
                    take(1, &mut at);
                }
                b'I' => {
                    // SAFETY: looking at most two bytes on, stopping at a zero.
                    let (a, b) = (peek(unsafe { at.add(1) }), if peek(unsafe { at.add(1) }) == 0 { 0 } else { peek(unsafe { at.add(2) }) });
                    if (a, b) == (b'6', b'4') {
                        spec.size = 8;
                        take(3, &mut at);
                    } else if (a, b) == (b'3', b'2') {
                        spec.size = 4;
                        take(3, &mut at);
                    } else {
                        spec.size = 8;
                        take(1, &mut at);
                    }
                }
                _ => break,
            }
        }
        let conversion = peek(at);
        if conversion == 0 {
            break;
        }
        // SAFETY: as above.
        at = unsafe { at.add(1) };
        match conversion {
            b'%' => emit(b'%'),
            b'd' | b'i' => {
                let value = signed(args.next(), spec.size);
                let sign = if value < 0 {
                    "-"
                } else if spec.plus {
                    "+"
                } else if spec.space {
                    " "
                } else {
                    ""
                };
                integer(&mut emit, spec, sign, value.unsigned_abs(), 10, false, "");
            }
            b'u' | b'o' | b'x' | b'X' | b'p' => {
                let (base, upper) = match conversion {
                    b'u' => (10, false),
                    b'o' => (8, false),
                    b'x' => (16, false),
                    _ => (16, true),
                };
                let mut spec = spec;
                if conversion == b'p' {
                    spec.size = 8;
                    spec.precision = Some(16);
                }
                let value = unsigned(args.next(), spec.size);
                let prefix = match conversion {
                    b'x' if spec.alternate && value != 0 => "0x",
                    b'X' if spec.alternate && value != 0 => "0X",
                    b'o' if spec.alternate => "0",
                    _ => "",
                };
                integer(&mut emit, spec, "", value, base, upper, prefix);
            }
            b'c' | b'C' => {
                let value = args.next();
                let mut text = [0u8; 4];
                let bytes: &[u8] = if spec.wide || conversion == b'C' {
                    char::from_u32(value as u16 as u32)
                        .unwrap_or(char::REPLACEMENT_CHARACTER)
                        .encode_utf8(&mut text)
                        .as_bytes()
                } else {
                    text[0] = value as u8;
                    &text[..1]
                };
                pad(&mut emit, spec, bytes);
            }
            b's' | b'S' => {
                let pointer = args.next();
                let wide = spec.wide || conversion == b'S';
                let mut text = String::new();
                if pointer == 0 {
                    text.push_str("(null)");
                } else if wide {
                    let units = (0..).map(|i| {
                        // SAFETY: the caller's promise: a zero-ended string.
                        unsafe { (pointer as *const u16).add(i).read_unaligned() }
                    });
                    for character in char::decode_utf16(units.take_while(|unit| *unit != 0)) {
                        if spec.precision.is_some_and(|most| text.len() >= most) {
                            break;
                        }
                        text.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
                    }
                } else {
                    let mut i = 0;
                    loop {
                        if spec.precision.is_some_and(|most| i >= most) {
                            break;
                        }
                        // SAFETY: the caller's promise; the precision, if
                        // given, bounds a string that has no zero.
                        let byte = unsafe { (pointer as *const u8).add(i).read() };
                        if byte == 0 {
                            break;
                        }
                        // Bytes, not characters: the text is kept as given.
                        unsafe { text.as_mut_vec().push(byte) };
                        i += 1;
                    }
                }
                let bytes = text.as_bytes();
                let bytes = match spec.precision {
                    Some(most) if wide => &bytes[..bytes.len().min(most)],
                    _ => bytes,
                };
                pad(&mut emit, spec, bytes);
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let value = f64::from_bits(args.next());
                let text = float(value, spec, conversion);
                let sign = if value.is_sign_negative() && !value.is_nan() {
                    "-"
                } else if spec.plus {
                    "+"
                } else if spec.space {
                    " "
                } else {
                    ""
                };
                let zero = spec.zero && value.is_finite();
                padded_number(&mut emit, spec, sign, "", text.as_bytes(), zero);
            }
            b'n' => {
                // Skipped, consuming its argument, as the Microsoft runtime
                // does unless asked otherwise: writing through it is how
                // format-string attacks write memory.
                args.next();
            }
            other => {
                // Not a conversion: shown as written.
                emit(b'%');
                emit(other);
            }
        }
    }
    count
}

fn signed(value: u64, size: u8) -> i64 {
    match size {
        1 => value as i8 as i64,
        2 => value as i16 as i64,
        4 => value as i32 as i64,
        _ => value as i64,
    }
}

fn unsigned(value: u64, size: u8) -> u64 {
    match size {
        1 => value as u8 as u64,
        2 => value as u16 as u64,
        4 => value as u32 as u64,
        _ => value,
    }
}

fn integer(emit: &mut dyn FnMut(u8), spec: Spec, sign: &str, value: u64, base: u64, upper: bool, prefix: &str) {
    let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut buffer = [0u8; 64];
    let mut len = 0;
    let mut rest = value;
    while rest != 0 {
        buffer[len] = digits[(rest % base) as usize];
        rest /= base;
        len += 1;
    }
    // A precision is the fewest digits; `%.0d` of 0 is nothing.
    let least = spec.precision.unwrap_or(1);
    let mut text = alloc::vec![b'0'; least.saturating_sub(len)];
    text.extend(buffer[..len].iter().rev());
    // `%#o` needs its leading zero only if there is not one already.
    let prefix = if prefix == "0" && text.first() == Some(&b'0') { "" } else { prefix };
    padded_number(emit, spec, sign, prefix, &text, spec.zero && spec.precision.is_none());
}

/// Sign, prefix and digits, padded to the width: with spaces before (or
/// after, `-`), or with zeroes after the sign and prefix.
fn padded_number(emit: &mut dyn FnMut(u8), spec: Spec, sign: &str, prefix: &str, digits: &[u8], zero: bool) {
    let len = sign.len() + prefix.len() + digits.len();
    let fill = spec.width.saturating_sub(len);
    if !spec.left && !zero {
        (0..fill).for_each(|_| emit(b' '));
    }
    sign.bytes().chain(prefix.bytes()).for_each(&mut *emit);
    if !spec.left && zero {
        (0..fill).for_each(|_| emit(b'0'));
    }
    digits.iter().for_each(|byte| emit(*byte));
    if spec.left {
        (0..fill).for_each(|_| emit(b' '));
    }
}

fn pad(emit: &mut dyn FnMut(u8), spec: Spec, bytes: &[u8]) {
    let fill = spec.width.saturating_sub(bytes.len());
    if !spec.left {
        (0..fill).for_each(|_| emit(b' '));
    }
    bytes.iter().for_each(|byte| emit(*byte));
    if spec.left {
        (0..fill).for_each(|_| emit(b' '));
    }
}

/// A float's digits, without its sign, as C formats them.
fn float(value: f64, spec: Spec, conversion: u8) -> String {
    let upper = conversion.is_ascii_uppercase();
    let magnitude = value.abs();
    let mut text = String::new();
    if value.is_nan() {
        text.push_str("nan");
    } else if value.is_infinite() {
        text.push_str("inf");
    } else {
        let precision = spec.precision.unwrap_or(6);
        match conversion.to_ascii_lowercase() {
            b'f' => {
                let _ = write!(text, "{magnitude:.precision$}");
                if spec.alternate && precision == 0 {
                    text.push('.');
                }
            }
            b'e' | b'a' => text = exponent(magnitude, precision, spec.alternate),
            _ => {
                // %g: %e's exponent picks the style; trailing zeroes go
                // unless `#`.
                let precision = precision.max(1);
                let shown = exponent(magnitude, precision - 1, false);
                let power: i32 = shown.rsplit(['e']).next().and_then(|e| e.parse().ok()).unwrap_or(0);
                text = if power < -4 || power >= precision as i32 {
                    exponent(magnitude, precision - 1, spec.alternate)
                } else {
                    let decimals = (precision as i32 - 1 - power).max(0) as usize;
                    let mut fixed = String::new();
                    let _ = write!(fixed, "{magnitude:.decimals$}");
                    fixed
                };
                if !spec.alternate && text.contains('.') {
                    let (mantissa, tail) = match text.find('e') {
                        Some(at) => (String::from(&text[..at]), String::from(&text[at..])),
                        None => (text.clone(), String::new()),
                    };
                    let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
                    text = String::from(mantissa) + &tail;
                }
            }
        }
    }
    if upper {
        text.make_ascii_uppercase();
    }
    text
}

/// `d.ddde+XX`, as C writes it: a sign and at least two exponent digits.
fn exponent(magnitude: f64, precision: usize, alternate: bool) -> String {
    let mut rust = String::new();
    let _ = write!(rust, "{magnitude:.precision$e}");
    let (mantissa, power) = rust.split_once('e').unwrap_or((&rust, "0"));
    let power: i32 = power.parse().unwrap_or(0);
    let mut text = String::from(mantissa);
    if alternate && precision == 0 {
        text.push('.');
    }
    let _ = write!(text, "e{}{:02}", if power < 0 { '-' } else { '+' }, power.unsigned_abs());
    text
}
