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

/// What [`lens_row`] does to the colour beyond the rim and the blend.
#[derive(Clone, Copy, Debug)]
pub struct Lens {
    /// `glass::saturate`'s chroma, in 4096ths, or `None` to leave chroma be.
    pub chroma: Option<i16>,
    /// The pane's alpha.
    pub alpha: u8,
}

/// The lens's last steps for a row of `0x00RRGGBB` pixels, two at a time:
/// `toned` is each pixel's backdrop through the tone curve, `shade` its
/// coverage, rim light and rim opacity (`covered | light << 8 | opacity <<
/// 16`). Saturates, darkens and lights by the rim, and blends the result
/// over `dst` by the coverage times `lens.alpha`, as `glass::saturate`, the
/// rim and [`crate::blend::Blend`] do one pixel at a time. A pixel with no
/// coverage keeps what `dst` had.
///
/// # Safety
/// `toned`, `shade` and `dst` each have `2 * pairs` pixels.
pub unsafe fn lens_row(toned: *const u32, shade: *const u32, dst: *mut u32, pairs: usize, lens: Lens) {
    if pairs == 0 {
        return;
    }
    let (saturate, chroma) = match lens.chroma {
        Some(chroma) => (1u32, chroma as u16 as u32),
        None => (0, 0),
    };
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            // Constants: zero; the luma weights (213, 715, 72 for red, green
            // and blue; pixels are b, g, r, x in memory); the chroma; 0x8081
            // for dividing by 255; 255; the alpha; 128; 1000's reciprocal;
            // and a mask that keeps b, g and r and clears x.
            "pxor xmm7, xmm7",
            "mov {t:e}, 0x02cb0048",
            "movd xmm8, {t:e}",
            "mov {t:e}, 0x000000d5",
            "movd xmm1, {t:e}",
            "punpckldq xmm8, xmm1",
            "punpcklqdq xmm8, xmm8",
            "movd xmm9, {chroma:e}",
            "pshuflw xmm9, xmm9, 0",
            "punpcklqdq xmm9, xmm9",
            "mov {t:e}, 0x80818081",
            "movd xmm10, {t:e}",
            "pshufd xmm10, xmm10, 0",
            "mov {t:e}, 0x00ff00ff",
            "movd xmm11, {t:e}",
            "pshufd xmm11, xmm11, 0",
            "movd xmm12, {alpha:e}",
            "pshuflw xmm12, xmm12, 0",
            "punpcklqdq xmm12, xmm12",
            "mov {t:e}, 0x00800080",
            "movd xmm13, {t:e}",
            "pshufd xmm13, xmm13, 0",
            "mov {t:e}, 268436",
            "movd xmm14, {t:e}",
            "pshufd xmm14, xmm14, 0",
            "mov {t:e}, -1",
            "movd xmm6, {t:e}",
            "mov {t:e}, 0xffff",
            "movd xmm1, {t:e}",
            "punpckldq xmm6, xmm1",
            "punpcklqdq xmm6, xmm6",
            "2:",
            "movq xmm0, qword ptr [{toned}]",
            "punpcklbw xmm0, xmm7",
            "test {saturate:e}, {saturate:e}",
            "jz 3f",
            // luma = (213 r + 715 g + 72 b) / 1000, in every lane of its pixel.
            "movdqa xmm1, xmm0",
            "pmaddwd xmm1, xmm8",
            "pshufd xmm2, xmm1, 0xb1",
            "paddd xmm1, xmm2",
            "pmuludq xmm1, xmm14",
            "psrlq xmm1, 28",
            "pshuflw xmm1, xmm1, 0",
            "pshufhw xmm1, xmm1, 0",
            // luma + (c - luma) * chroma / 4096, rounded toward zero.
            "movdqa xmm2, xmm0",
            "psubw xmm2, xmm1",
            "movdqa xmm3, xmm2",
            "pmullw xmm2, xmm9",
            "pmulhw xmm3, xmm9",
            "movdqa xmm4, xmm2",
            "punpcklwd xmm2, xmm3",
            "punpckhwd xmm4, xmm3",
            "movdqa xmm5, xmm2",
            "psrad xmm5, 31",
            "psrld xmm5, 20",
            "paddd xmm2, xmm5",
            "psrad xmm2, 12",
            "movdqa xmm5, xmm4",
            "psrad xmm5, 31",
            "psrld xmm5, 20",
            "paddd xmm4, xmm5",
            "psrad xmm4, 12",
            "packssdw xmm2, xmm4",
            "paddw xmm2, xmm1",
            "pmaxsw xmm2, xmm7",
            "pminsw xmm2, xmm11",
            "movdqa xmm0, xmm2",
            "pand xmm0, xmm6",
            "3:",
            // The rim: c * (255 - opacity) / 255 + light, at most 255.
            "movq xmm3, qword ptr [{shade}]",
            "punpcklbw xmm3, xmm7",
            "pshuflw xmm4, xmm3, 0xaa",
            "pshufhw xmm4, xmm4, 0xaa",
            "pshuflw xmm5, xmm3, 0x55",
            "pshufhw xmm5, xmm5, 0x55",
            "pshuflw xmm3, xmm3, 0",
            "pshufhw xmm3, xmm3, 0",
            "movdqa xmm2, xmm11",
            "psubw xmm2, xmm4",
            "pmullw xmm0, xmm2",
            "pmulhuw xmm0, xmm10",
            "psrlw xmm0, 7",
            "paddw xmm0, xmm5",
            "pminsw xmm0, xmm11",
            "pand xmm0, xmm6",
            // alpha = covered * pane's alpha / 255, rounded.
            "pmullw xmm3, xmm12",
            "paddw xmm3, xmm13",
            "movdqa xmm2, xmm3",
            "psrlw xmm2, 8",
            "paddw xmm3, xmm2",
            "psrlw xmm3, 8",
            // Over dst: (d * (255 - a) + c * a + 128) / 255, rounded.
            "movq xmm2, qword ptr [{dst}]",
            "punpcklbw xmm2, xmm7",
            "movdqa xmm4, xmm11",
            "psubw xmm4, xmm3",
            "pmullw xmm2, xmm4",
            "pmullw xmm0, xmm3",
            "paddw xmm0, xmm2",
            "paddw xmm0, xmm13",
            "movdqa xmm2, xmm0",
            "psrlw xmm2, 8",
            "paddw xmm0, xmm2",
            "psrlw xmm0, 8",
            "packuswb xmm0, xmm0",
            "movq qword ptr [{dst}], xmm0",
            "add {toned}, 8",
            "add {shade}, 8",
            "add {dst}, 8",
            "dec {n}",
            "jnz 2b",
            toned = inout(reg) toned => _,
            shade = inout(reg) shade => _,
            dst = inout(reg) dst => _,
            n = inout(reg) pairs => _,
            saturate = in(reg) saturate,
            chroma = in(reg) chroma,
            alpha = in(reg) lens.alpha as u32,
            t = out(reg) _,
            #[cfg(target_feature = "sse2")] out("xmm0") _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm4") _,
            #[cfg(target_feature = "sse2")] out("xmm5") _,
            #[cfg(target_feature = "sse2")] out("xmm6") _,
            #[cfg(target_feature = "sse2")] out("xmm7") _,
            #[cfg(target_feature = "sse2")] out("xmm8") _,
            #[cfg(target_feature = "sse2")] out("xmm9") _,
            #[cfg(target_feature = "sse2")] out("xmm10") _,
            #[cfg(target_feature = "sse2")] out("xmm11") _,
            #[cfg(target_feature = "sse2")] out("xmm12") _,
            #[cfg(target_feature = "sse2")] out("xmm13") _,
            #[cfg(target_feature = "sse2")] out("xmm14") _,
            options(nostack),
        );
    }
}

/// Frost behind its veil, four `0x00RRGGBB` pixels at a time: each channel
/// of `frost` becomes `(blend * veil + frost * (255 - veil)) / 255`, rounded
/// down, where `blend` is the lighter of it and `fill`'s (the darker, when
/// `dark`).
///
/// # Safety
/// `frost` and `fill` each have `4 * groups` pixels.
pub unsafe fn veil_mix(frost: *mut u32, fill: *const u32, groups: usize, veil: u8, dark: bool) {
    if groups == 0 {
        return;
    }
    // SAFETY: the caller's bounds; every access is unaligned-safe.
    unsafe {
        asm!(
            "pxor xmm7, xmm7",
            "movd xmm8, {veil:e}",
            "pshuflw xmm8, xmm8, 0",
            "punpcklqdq xmm8, xmm8",
            "mov {t:e}, 0x00ff00ff",
            "movd xmm9, {t:e}",
            "pshufd xmm9, xmm9, 0",
            "psubw xmm9, xmm8",
            "mov {t:e}, 0x80818081",
            "movd xmm10, {t:e}",
            "pshufd xmm10, xmm10, 0",
            "mov {t:e}, 0x00ffffff",
            "movd xmm11, {t:e}",
            "pshufd xmm11, xmm11, 0",
            "2:",
            "movdqu xmm0, [{frost}]",
            "movdqu xmm1, [{fill}]",
            "test {dark:e}, {dark:e}",
            "jz 3f",
            "pminub xmm1, xmm0",
            "jmp 4f",
            "3:",
            "pmaxub xmm1, xmm0",
            "4:",
            "movdqa xmm2, xmm0",
            "punpcklbw xmm0, xmm7",
            "punpckhbw xmm2, xmm7",
            "movdqa xmm3, xmm1",
            "punpcklbw xmm1, xmm7",
            "punpckhbw xmm3, xmm7",
            "pmullw xmm1, xmm8",
            "pmullw xmm3, xmm8",
            "pmullw xmm0, xmm9",
            "pmullw xmm2, xmm9",
            "paddw xmm0, xmm1",
            "paddw xmm2, xmm3",
            "pmulhuw xmm0, xmm10",
            "pmulhuw xmm2, xmm10",
            "psrlw xmm0, 7",
            "psrlw xmm2, 7",
            "packuswb xmm0, xmm2",
            "pand xmm0, xmm11",
            "movdqu [{frost}], xmm0",
            "add {frost}, 16",
            "add {fill}, 16",
            "dec {n}",
            "jnz 2b",
            frost = inout(reg) frost => _,
            fill = inout(reg) fill => _,
            n = inout(reg) groups => _,
            veil = in(reg) veil as u32,
            dark = in(reg) dark as u32,
            t = out(reg) _,
            #[cfg(target_feature = "sse2")] out("xmm0") _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            #[cfg(target_feature = "sse2")] out("xmm7") _,
            #[cfg(target_feature = "sse2")] out("xmm8") _,
            #[cfg(target_feature = "sse2")] out("xmm9") _,
            #[cfg(target_feature = "sse2")] out("xmm10") _,
            #[cfg(target_feature = "sse2")] out("xmm11") _,
            options(nostack),
        );
    }
}

/// The weights [`quarter_bilinear`] takes: for `wx` and `wy` (quarters of a
/// pixel, 0 to 3), the top two pixels' and the bottom two's, four lanes each.
pub static QUARTER_WEIGHTS: [[[u16; 8]; 2]; 16] = {
    let mut table = [[[0u16; 8]; 2]; 16];
    let mut at = 0;
    while at < 16 {
        let (wx, wy) = ((at / 4) as u16, (at % 4) as u16);
        let mut lane = 0;
        while lane < 4 {
            table[at][0][lane] = (4 - wx) * (4 - wy);
            table[at][0][lane + 4] = wx * (4 - wy);
            table[at][1][lane] = (4 - wx) * wy;
            table[at][1][lane + 4] = wx * wy;
            lane += 1;
        }
        at += 1;
    }
    table
};

/// Between four `0x00RRGGBB` pixels, `a` and `b` above `c` and `d`, a
/// quarter-pixel `wx` along and `wy` down (each 0 to 3): every channel
/// `((a (4 - wx) + b wx) (4 - wy) + (c (4 - wx) + d wx) wy) / 16`, rounded
/// down.
#[inline]
pub fn quarter_bilinear([a, b, c, d]: [u32; 4], wx: usize, wy: usize) -> u32 {
    let weights = &QUARTER_WEIGHTS[(wx & 3) * 4 + (wy & 3)];
    let out: u32;
    // SAFETY: `weights` is 32 readable bytes; the rest is registers.
    unsafe {
        asm!(
            "pxor xmm3, xmm3",
            "movd xmm0, {a:e}",
            "movd xmm1, {b:e}",
            "punpckldq xmm0, xmm1",
            "punpcklbw xmm0, xmm3",
            "movd xmm1, {c:e}",
            "movd xmm2, {d:e}",
            "punpckldq xmm1, xmm2",
            "punpcklbw xmm1, xmm3",
            "movdqu xmm2, [{w}]",
            "pmullw xmm0, xmm2",
            "movdqu xmm2, [{w} + 16]",
            "pmullw xmm1, xmm2",
            "paddw xmm0, xmm1",
            "pshufd xmm1, xmm0, 0xee",
            "paddw xmm0, xmm1",
            "psrlw xmm0, 4",
            "packuswb xmm0, xmm0",
            "movd {out:e}, xmm0",
            a = in(reg) a,
            b = in(reg) b,
            c = in(reg) c,
            d = in(reg) d,
            w = in(reg) weights.as_ptr(),
            out = lateout(reg) out,
            #[cfg(target_feature = "sse2")] out("xmm0") _,
            #[cfg(target_feature = "sse2")] out("xmm1") _,
            #[cfg(target_feature = "sse2")] out("xmm2") _,
            #[cfg(target_feature = "sse2")] out("xmm3") _,
            options(nostack, readonly, pure),
        );
    }
    out
}
