//! SSE2 loops for the box blur, written by hand.
//!
//! Corrode's targets are soft-float: the compiler never puts anything in an
//! XMM register, so nothing it writes is vectorised. These loops are, in
//! `asm!`. Each block is whole: it makes no call and keeps nothing in an XMM
//! register past its end, so the only thing that must preserve them is a task
//! switch, which the kernel does (`arch::x86_64::fpu`).
//!
//! A pixel is four bytes, `0x00RRGGBB`, and here four 16-bit lanes. A sum of
//! up to `2 * MAX_BLUR_RADIUS + 1` bytes is at most `255 * 257 = 65535`, so a
//! lane holds it. The top byte is summed along with the rest and masked off
//! on the way out.
//!
//! On a build that can allocate XMM registers (the host's, for tests), every
//! block names the ones it uses; a soft-float build has no XMM register class
//! to name them in, and needs no names, since nothing else uses them.

use core::arch::asm;

/// What [`mean_row`] and [`rows`] need to divide every lane by `span`:
/// `n / span` is `(t + ((n - t) >> 1)) >> shift`, `t` the high half of
/// `n * multiplier`. Exact for every 16-bit `n` (Granlund and Montgomery,
/// "Division by invariant integers using multiplication", 1994).
#[derive(Clone, Copy, Debug)]
pub struct Divisor {
    multiplier: u16,
    shift: u32,
}

impl Divisor {
    /// For `span` from 2 to 65535.
    pub fn new(span: u32) -> Self {
        let log = 32 - (span - 1).leading_zeros();
        let multiplier = ((1u64 << 16) * ((1u64 << log) - span as u64) / span as u64 + 1) as u16;
        Self { multiplier, shift: log - 1 }
    }

    /// What one lane works out, for checking.
    #[cfg(test)]
    pub fn divide(self, n: u16) -> u16 {
        let t = ((n as u32 * self.multiplier as u32) >> 16) as u16;
        (t + ((n - t) >> 1)) >> self.shift
    }
}

/// `sums[i] += pixel's lanes`, for four pixels per group, `groups` groups.
///
/// # Safety
/// `sums` has `16 * groups` lanes and `row` `4 * groups` pixels.
pub unsafe fn add_row(sums: *mut u16, row: *const u32, groups: usize) {
    if groups == 0 {
        return;
    }
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            "pxor xmm7, xmm7",
            "2:",
            "movdqu xmm1, [{row}]",
            "movdqa xmm2, xmm1",
            "punpcklbw xmm1, xmm7",
            "punpckhbw xmm2, xmm7",
            "movdqu xmm3, [{sums}]",
            "movdqu xmm4, [{sums} + 16]",
            "paddw xmm3, xmm1",
            "paddw xmm4, xmm2",
            "movdqu [{sums}], xmm3",
            "movdqu [{sums} + 16], xmm4",
            "add {row}, 16",
            "add {sums}, 32",
            "dec {n}",
            "jnz 2b",
            row = inout(reg) row => _,
            sums = inout(reg) sums => _,
            n = inout(reg) groups => _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm4") _,
            #[cfg(target_feature = "sse2")] out("xmm7") _,
            options(nostack),
        );
    }
}

/// `sums[i] += incoming's lanes - leaving's`, four pixels per group.
///
/// # Safety
/// `sums` has `16 * groups` lanes; `incoming` and `leaving` `4 * groups`
/// pixels each.
pub unsafe fn step_row(sums: *mut u16, incoming: *const u32, leaving: *const u32, groups: usize) {
    if groups == 0 {
        return;
    }
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            "pxor xmm7, xmm7",
            "2:",
            "movdqu xmm1, [{incoming}]",
            "movdqa xmm2, xmm1",
            "punpcklbw xmm1, xmm7",
            "punpckhbw xmm2, xmm7",
            "movdqu xmm5, [{leaving}]",
            "movdqa xmm6, xmm5",
            "punpcklbw xmm5, xmm7",
            "punpckhbw xmm6, xmm7",
            "movdqu xmm3, [{sums}]",
            "movdqu xmm4, [{sums} + 16]",
            "paddw xmm3, xmm1",
            "paddw xmm4, xmm2",
            "psubw xmm3, xmm5",
            "psubw xmm4, xmm6",
            "movdqu [{sums}], xmm3",
            "movdqu [{sums} + 16], xmm4",
            "add {incoming}, 16",
            "add {leaving}, 16",
            "add {sums}, 32",
            "dec {n}",
            "jnz 2b",
            incoming = inout(reg) incoming => _,
            leaving = inout(reg) leaving => _,
            sums = inout(reg) sums => _,
            n = inout(reg) groups => _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm4") _,
            #[cfg(target_feature = "sse2")] out("xmm5") _,
            #[cfg(target_feature = "sse2")] out("xmm6") _,
            #[cfg(target_feature = "sse2")] out("xmm7") _,
            options(nostack),
        );
    }
}

/// `out[i]` = each lane of `sums[i]` divided, as a `0x00RRGGBB` pixel, four
/// pixels per group.
///
/// # Safety
/// `sums` has `16 * groups` lanes and `out` room for `4 * groups` pixels.
pub unsafe fn mean_row(sums: *const u16, out: *mut u32, groups: usize, divisor: Divisor) {
    if groups == 0 {
        return;
    }
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            "movd xmm8, {multiplier:e}",
            "pshuflw xmm8, xmm8, 0",
            "punpcklqdq xmm8, xmm8",
            "movd xmm9, {shift:e}",
            "mov {shift:e}, 0x00ffffff",
            "movd xmm10, {shift:e}",
            "pshufd xmm10, xmm10, 0",
            "2:",
            "movdqu xmm1, [{sums}]",
            "movdqu xmm2, [{sums} + 16]",
            // t = high(n * m); q = (t + ((n - t) >> 1)) >> shift.
            "movdqa xmm3, xmm1",
            "movdqa xmm4, xmm2",
            "pmulhuw xmm3, xmm8",
            "pmulhuw xmm4, xmm8",
            "psubw xmm1, xmm3",
            "psubw xmm2, xmm4",
            "psrlw xmm1, 1",
            "psrlw xmm2, 1",
            "paddw xmm1, xmm3",
            "paddw xmm2, xmm4",
            "psrlw xmm1, xmm9",
            "psrlw xmm2, xmm9",
            "packuswb xmm1, xmm2",
            "pand xmm1, xmm10",
            "movdqu [{out}], xmm1",
            "add {sums}, 32",
            "add {out}, 16",
            "dec {n}",
            "jnz 2b",
            multiplier = in(reg) divisor.multiplier as u32,
            shift = inout(reg) divisor.shift => _,
            sums = inout(reg) sums => _,
            out = inout(reg) out => _,
            n = inout(reg) groups => _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm4") _,
            #[cfg(target_feature = "sse2")] out("xmm8") _,
            #[cfg(target_feature = "sse2")] out("xmm9") _,
            #[cfg(target_feature = "sse2")] out("xmm10") _,
            options(nostack),
        );
    }
}

/// Box-blur two rows at once, one in the low lanes and one in the high.
///
/// Each of `a` and `b` is a row with its edges already repeated: `width +
/// span` pixels, where output pixel `i` is the mean of `a[i..i + span]`.
/// The two may be the same row.
///
/// # Safety
/// `a` and `b` have `width + span` pixels; `out_a` and `out_b` room for
/// `width`. `span` is at least 1.
pub unsafe fn rows(
    a: *const u32,
    b: *const u32,
    out_a: *mut u32,
    out_b: *mut u32,
    width: usize,
    span: usize,
    divisor: Divisor,
) {
    if width == 0 {
        return;
    }
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            "movd xmm8, {multiplier:e}",
            "pshuflw xmm8, xmm8, 0",
            "punpcklqdq xmm8, xmm8",
            "movd xmm9, {shift:e}",
            "mov {shift:e}, 0x00ffffff",
            "movd xmm10, {shift:e}",
            "pshufd xmm10, xmm10, 0",
            "pxor xmm7, xmm7",
            // The first box: span pixels of each row.
            "pxor xmm0, xmm0",
            "xor {i}, {i}",
            "2:",
            "movd xmm1, [{a} + {i} * 4]",
            "movd xmm2, [{b} + {i} * 4]",
            "punpckldq xmm1, xmm2",
            "punpcklbw xmm1, xmm7",
            "paddw xmm0, xmm1",
            "inc {i}",
            "cmp {i}, {span}",
            "jne 2b",
            // Then each pixel: write the mean, slide the box one along.
            "shl {span}, 2",
            "3:",
            "movdqa xmm1, xmm0",
            "movdqa xmm3, xmm0",
            "pmulhuw xmm3, xmm8",
            "psubw xmm1, xmm3",
            "psrlw xmm1, 1",
            "paddw xmm1, xmm3",
            "psrlw xmm1, xmm9",
            "packuswb xmm1, xmm1",
            "pand xmm1, xmm10",
            "movd [{out_a}], xmm1",
            "psrlq xmm1, 32",
            "movd [{out_b}], xmm1",
            "movd xmm1, [{a} + {span}]",
            "movd xmm2, [{b} + {span}]",
            "punpckldq xmm1, xmm2",
            "punpcklbw xmm1, xmm7",
            "movd xmm4, [{a}]",
            "movd xmm5, [{b}]",
            "punpckldq xmm4, xmm5",
            "punpcklbw xmm4, xmm7",
            "paddw xmm0, xmm1",
            "psubw xmm0, xmm4",
            "add {a}, 4",
            "add {b}, 4",
            "add {out_a}, 4",
            "add {out_b}, 4",
            "dec {n}",
            "jnz 3b",
            multiplier = in(reg) divisor.multiplier as u32,
            shift = inout(reg) divisor.shift => _,
            a = inout(reg) a => _,
            b = inout(reg) b => _,
            out_a = inout(reg) out_a => _,
            out_b = inout(reg) out_b => _,
            span = inout(reg) span => _,
            n = inout(reg) width => _,
            i = out(reg) _,
            #[cfg(target_feature = "sse2")] out("xmm0") _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm4") _,
            #[cfg(target_feature = "sse2")] out("xmm5") _,
            #[cfg(target_feature = "sse2")] out("xmm7") _,
            #[cfg(target_feature = "sse2")] out("xmm8") _,
            #[cfg(target_feature = "sse2")] out("xmm9") _,
            #[cfg(target_feature = "sse2")] out("xmm10") _,
            options(nostack),
        );
    }
}
