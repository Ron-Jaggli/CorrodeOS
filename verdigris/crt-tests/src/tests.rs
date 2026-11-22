extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::format::{format, Args};
use crate::text::{number, split, to_long, to_unsigned_long, Number};

// ---------------------------------------------------------------- printf

/// One argument slot, as a variadic call leaves it.
enum A<'a> {
    I(i64),
    U(u64),
    F(f64),
    S(&'a [u8]),
    W(&'a [u16]),
}

/// `format` run on `text` with `args`: what it wrote, checked against the
/// count it returned.
fn printf(text: &str, args: &[A]) -> String {
    let text: Vec<u8> = text.bytes().chain([0]).collect();
    let slots: Vec<u64> = args
        .iter()
        .map(|arg| match arg {
            A::I(v) => *v as u64,
            A::U(v) => *v,
            A::F(v) => v.to_bits(),
            A::S(s) => s.as_ptr() as u64,
            A::W(s) => s.as_ptr() as u64,
        })
        .collect();
    let mut out = Vec::new();
    // SAFETY: a zero-ended format, and a slot for each conversion; the
    // strings are zero-ended.
    let count = unsafe { format(text.as_ptr(), &mut Args(slots.as_ptr()), &mut |b| out.push(b)) };
    assert_eq!(count, out.len(), "the count for {:?}", core::str::from_utf8(&text));
    String::from_utf8(out).expect("UTF-8")
}

#[test]
fn integers() {
    use A::*;
    let cases: &[(&str, &[A], &str)] = &[
        ("%d", &[I(42)], "42"),
        ("%i", &[I(-42)], "-42"),
        ("%5d|%-5d|", &[I(42), I(-7)], "   42|-7   |"),
        ("%05d", &[I(-42)], "-0042"),
        ("%+d % d", &[I(7), I(7)], "+7  7"),
        ("%.3d|%5.3d|%-5.3d|", &[I(7), I(7), I(7)], "007|  007|007  |"),
        ("%.0d|", &[I(0)], "|"),
        ("%u", &[U(4_000_000_000)], "4000000000"),
        ("%x %X %#x %#X", &[U(0xbeef), U(0xbeef), U(255), U(255)], "beef BEEF 0xff 0XFF"),
        ("%o %#o", &[U(8), U(8)], "10 010"),
        ("%x", &[I(-1)], "ffffffff"),
        ("%d", &[U(0xdead_0000_0000_0005)], "5"),
        ("%lld", &[I(i64::MIN)], "-9223372036854775808"),
        ("%llu %I64u", &[U(u64::MAX), U(u64::MAX)], "18446744073709551615 18446744073709551615"),
        ("%I64x %I32x", &[U(0x1_0000_0001), U(0x1_0000_0001)], "100000001 1"),
        ("%zu %Iu", &[U(1 << 40), U(1 << 40)], "1099511627776 1099511627776"),
        ("%hhd %hd", &[I(300), I(70_000)], "44 4464"),
        ("%hhu %hu", &[I(-1), I(-1)], "255 65535"),
        ("%*d|%-*d|", &[I(5), I(42), I(4), I(1)], "   42|1   |"),
        ("%*d|", &[I(-4), I(1)], "1   |"),
        ("%.*d", &[I(4), I(9)], "0009"),
        ("%p", &[U(0x1234)], "0000000000001234"),
        ("%p", &[U(0xdead_beef_cafe)], "0000DEADBEEFCAFE"),
    ];
    for (text, args, want) in cases {
        assert_eq!(printf(text, args), *want, "{text}");
    }
}

#[test]
fn floats() {
    use A::*;
    let cases: &[(&str, &[A], &str)] = &[
        ("%f", &[F(1.5)], "1.500000"),
        ("%.2f", &[F(3.14159)], "3.14"),
        ("%05.1f", &[F(3.14159)], "003.1"),
        ("%08.3f", &[F(2.5)], "0002.500"),
        ("%-8.2f|", &[F(-1.25)], "-1.25   |"),
        ("%+.1f", &[F(2.0)], "+2.0"),
        ("%.0f", &[F(7.0)], "7"),
        ("%#.0f", &[F(7.0)], "7."),
        ("%.*f", &[I(3), F(0.1)], "0.100"),
        ("%f", &[F(1e20)], "100000000000000000000.000000"),
        ("%e", &[F(12345.678)], "1.234568e+04"),
        ("%E", &[F(1.5)], "1.500000E+00"),
        ("%.2e", &[F(0.000123)], "1.23e-04"),
        ("%e", &[F(0.0)], "0.000000e+00"),
        ("%e", &[F(1e100)], "1.000000e+100"),
        ("%g", &[F(0.0001)], "0.0001"),
        ("%g", &[F(100000.0)], "100000"),
        ("%g", &[F(1000000.0)], "1e+06"),
        ("%g", &[F(1.5)], "1.5"),
        ("%g", &[F(0.00001)], "1e-05"),
        ("%G", &[F(1e-10)], "1E-10"),
        ("%.3g", &[F(3.14159)], "3.14"),
        ("%#g", &[F(1.5)], "1.50000"),
        ("%10.4g|", &[F(123.456)], "     123.5|"),
        ("%g", &[F(-0.5)], "-0.5"),
    ];
    for (text, args, want) in cases {
        assert_eq!(printf(text, args), *want, "{text}");
    }
}

#[test]
fn text_and_the_rest() {
    use A::*;
    let hi: Vec<u16> = "hi there".encode_utf16().chain([0]).collect();
    let cases: &[(&str, &[A], &str)] = &[
        ("%s", &[S(b"copper\0")], "copper"),
        ("%5s|%-5s|", &[S(b"ab\0"), S(b"ab\0")], "   ab|ab   |"),
        ("%.2s", &[S(b"xyz\0")], "xy"),
        ("%.*s", &[I(1), S(b"xyz\0")], "x"),
        ("%s", &[U(0)], "(null)"),
        ("%c%c", &[I(b'Z' as i64), I(b'!' as i64)], "Z!"),
        ("%3c|", &[I(b'a' as i64)], "  a|"),
        ("%ls", &[W(&hi)], "hi there"),
        ("%S", &[W(&hi)], "hi there"),
        ("%.2ls", &[W(&hi)], "hi"),
        ("%C", &[I(b'Q' as i64)], "Q"),
        ("100%%", &[], "100%"),
        ("a%nb", &[U(0)], "ab"),
        ("no conversions", &[], "no conversions"),
        ("", &[], ""),
        ("%d-%s-%x-%c", &[I(1), S(b"two\0"), U(3), I(b'4' as i64)], "1-two-3-4"),
    ];
    for (text, args, want) in cases {
        assert_eq!(printf(text, args), *want, "{text}");
    }
}

// ---------------------------------------------------------------- the command line

fn args(line: &str) -> Vec<String> {
    split(line.as_bytes()).into_iter().map(|a| String::from_utf8(a).expect("UTF-8")).collect()
}

#[test]
fn a_command_line_splits_as_microsofts_runtime_splits_it() {
    let cases: &[(&str, &[&str])] = &[
        ("prog", &["prog"]),
        ("", &[""]),
        ("prog a b", &["prog", "a", "b"]),
        ("prog   a \t b  ", &["prog", "a", "b"]),
        (r#""C:\dir name\prog.exe" x"#, &[r"C:\dir name\prog.exe", "x"]),
        // The name takes quotes but not backslash rules.
        (r#"C:\a\\"b c" d"#, &[r"C:\a\\b c", "d"]),
        (r#"p "two words""#, &["p", "two words"]),
        (r#"p """#, &["p", ""]),
        (r#"p "" x"#, &["p", "", "x"]),
        (r"p a\b", &["p", r"a\b"]),
        (r"p a\\b", &["p", r"a\\b"]),
        (r#"p a\"b"#, &["p", r#"a"b"#]),
        (r#"p a\\"b c""#, &["p", r"a\b c"]),
        (r#"p a\\\"b"#, &["p", r#"a\"b"#]),
        (r#"p "C:\dir name\\""#, &["p", r"C:\dir name\"]),
        (r#"p "say \"hi\"""#, &["p", r#"say "hi""#]),
        (r#"p "a""b""#, &["p", r#"a"b"#]),
        (r#"p a"b c"d"#, &["p", "ab cd"]),
        (r#"p "unterminated arg"#, &["p", "unterminated arg"]),
        (r"p trailing\", &["p", r"trailing\"]),
        // What the terminal sends for args.exe's run in win-test.
        (
            r#"C:\args.exe plain "two words" "" "a\\\"b" "C:\dir name\\" "say \"hi\"" 7"#,
            &[r"C:\args.exe", "plain", "two words", "", r#"a\"b"#, r"C:\dir name\", r#"say "hi""#, "7"],
        ),
    ];
    for (line, want) in cases {
        let want: Vec<String> = want.iter().map(|s| String::from(*s)).collect();
        assert_eq!(args(line), want, "{line}");
    }
}

// ---------------------------------------------------------------- numbers

fn long(text: &str, base: u32) -> (i32, bool, usize) {
    let read = number(text.as_bytes(), base);
    let (value, range) = to_long(&read);
    (value, range, read.len)
}

fn unsigned(text: &str, base: u32) -> (u32, bool, usize) {
    let read = number(text.as_bytes(), base);
    let (value, range) = to_unsigned_long(&read);
    (value, range, read.len)
}

#[test]
fn numbers_read_as_strtol_reads_them() {
    assert_eq!(long("42", 10), (42, false, 2));
    assert_eq!(long("  +77 rest", 10), (77, false, 5));
    assert_eq!(long("  -123xyz", 10), (-123, false, 6));
    assert_eq!(long("0x1F", 16), (31, false, 4));
    assert_eq!(long("1F", 16), (31, false, 2));
    assert_eq!(long("0x10", 0), (16, false, 4));
    assert_eq!(long("010", 0), (8, false, 3));
    assert_eq!(long("10", 0), (10, false, 2));
    assert_eq!(long("1011", 2), (11, false, 4));
    assert_eq!(long("z", 36), (35, false, 1));
    // No number: nothing read, so `endptr` stays at the start.
    assert_eq!(long("zz", 10), (0, false, 0));
    assert_eq!(long("", 10), (0, false, 0));
    assert_eq!(long("  -", 10), (0, false, 0));
    // "0x" with no hex digit after it is the number 0, then "x".
    assert_eq!(long("0x", 16), (0, false, 1));
    assert_eq!(long("0xg", 0), (0, false, 1));
    // An octal digit run stops at 8.
    assert_eq!(long("08", 0), (0, false, 1));
    // Bases that are not bases.
    assert_eq!(long("10", 1), (0, false, 0));
    assert_eq!(long("10", 37), (0, false, 0));
    // A 32-bit long's edges.
    assert_eq!(long("2147483647", 10), (i32::MAX, false, 10));
    assert_eq!(long("-2147483648", 10), (i32::MIN, false, 11));
    assert_eq!(long("2147483648", 10), (i32::MAX, true, 10));
    assert_eq!(long("-2147483649", 10), (i32::MIN, true, 11));
    assert_eq!(long("99999999999", 10), (i32::MAX, true, 11));
    assert_eq!(long("999999999999999999999999999", 10), (i32::MAX, true, 27));
    assert_eq!(long("-999999999999999999999999999", 10), (i32::MIN, true, 28));
    assert_eq!(number(b"\t\n\x0b\x0c\r 5", 10), Number { negative: false, magnitude: Some(5), len: 7 });
}

#[test]
fn numbers_read_as_strtoul_reads_them() {
    assert_eq!(unsigned("4294967295", 10), (u32::MAX, false, 10));
    assert_eq!(unsigned("4294967296", 10), (u32::MAX, true, 10));
    assert_eq!(unsigned("-1", 10), (u32::MAX, false, 2));
    assert_eq!(unsigned("-4294967295", 10), (1, false, 11));
    assert_eq!(unsigned("0xffffffff", 0), (u32::MAX, false, 10));
    assert_eq!(unsigned("junk", 10), (0, false, 0));
    assert_eq!(unsigned("99999999999999999999999", 10), (u32::MAX, true, 23));
}

#[test]
fn random_text_never_panics() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let alphabet = b" \t\"\\abc019xX+-";
    for _ in 0..20_000 {
        let len = (next() % 24) as usize;
        let line: Vec<u8> = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
        let parts = split(&line);
        assert!(!parts.is_empty());
        let read = number(&line, (next() % 38) as u32);
        assert!(read.len <= line.len());
    }
    // Formats made of random pieces, with arguments that are all plain
    // numbers (no `%s`, which would read a pointer), never panic either.
    let pieces = ["%", "d", "x", "5", ".", "*", "-", "+", "#", "0", "l", "h", "I64", "f", "e", "g", "c", "u", "o", " ", "a"];
    let slots = vec![7u64; 64];
    for _ in 0..20_000 {
        let mut text = String::new();
        for _ in 0..(next() % 10) {
            text.push_str(pieces[(next() % pieces.len() as u64) as usize]);
        }
        text.push('\0');
        let mut out = Vec::new();
        // SAFETY: zero-ended; 64 numeric slots is more than any of these
        // formats asks for, and none has a string conversion.
        unsafe { format(text.as_ptr(), &mut Args(slots.as_ptr()), &mut |b| out.push(b)) };
    }
}
