//! AES-128 with a key that lives in a register and nowhere else.
//!
//! The secure core holds its key in `xmm15`. That is only sound because of
//! where it runs:
//!
//! * The kernel is built soft-float. The compiler never allocates an XMM
//!   register, and the trap stubs save general-purpose registers only, so no
//!   context switch or interrupt frame ever writes `xmm15` to memory.
//! * Nothing else runs on the secure core, so nothing else executes an SSE
//!   instruction there either. (The firmware calls in `efi_log` do, on the
//!   CPU that makes them. The one way that could be this CPU is its own panic
//!   or fatal trap, and `on_fatal` zeroes the key before either gets there.)
//!
//! The key goes from RDRAND into `xmm15` inside one `asm!` block, through
//! `rax`, which is cleared before the block ends. Encryption uses AES-NI and
//! expands the key schedule round by round in `xmm1`-`xmm3` instead of storing
//! it, so no round key is ever written to memory. The scratch registers are
//! zeroed before each block returns.
//!
//! What this does not cover: anything with ring 0 on this core could read the
//! register, so the kernel and the code here are still trusted; and SMM, below
//! the kernel, saves its own copy of whatever state it likes. Those are the
//! hypervisor and firmware problems from the design, not this module's.

/// Whether this CPU has everything the module uses: AES-NI, RDRAND, and
/// SSE4.1 for `pinsrq`.
pub fn supported() -> bool {
    const SSE41: u32 = 1 << 19;
    const AES: u32 = 1 << 25;
    const RDRAND: u32 = 1 << 30;
    let ecx: u32;
    // SAFETY: `cpuid` leaf 1 exists on every x86_64 CPU. rbx is reserved by
    // the compiler, so it is saved and restored around the instruction.
    unsafe {
        core::arch::asm!(
            "mov {saved}, rbx",
            "cpuid",
            "mov rbx, {saved}",
            saved = out(reg) _,
            inout("eax") 1u32 => _,
            inout("ecx") 0u32 => ecx,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    ecx & (SSE41 | AES | RDRAND) == SSE41 | AES | RDRAND
}

/// One step of the AES-128 key schedule, in place in `xmm1`, with the round
/// constant `$rcon`. Uses `xmm2` and `xmm3` as scratch.
macro_rules! expand {
    ($rcon:literal) => {
        concat!(
            "aeskeygenassist xmm2, xmm1, ",
            $rcon,
            "\n",
            "pshufd xmm2, xmm2, 0xff\n",
            "movdqa xmm3, xmm1\n",
            "pslldq xmm3, 4\n",
            "pxor xmm1, xmm3\n",
            "pslldq xmm3, 4\n",
            "pxor xmm1, xmm3\n",
            "pslldq xmm3, 4\n",
            "pxor xmm1, xmm3\n",
            "pxor xmm1, xmm2\n",
        )
    };
}

/// Encrypt one block in place with the key in `xmm15`.
///
/// # Safety
/// SSE must be enabled on this CPU, [`supported`] must be true, and `xmm15`
/// must hold the key: run only on the secure core.
pub unsafe fn encrypt_block(block: &mut [u8; 16]) {
    // SAFETY: forwarded from the caller. `block` is 16 bytes, read and written
    // unaligned. XMM registers are not declared as clobbers because this
    // soft-float build has no XMM register class and never keeps a value in
    // one; the scratch ones are zeroed before the block ends regardless.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, xmmword ptr [{block}]",
            "movdqa xmm1, xmm15",
            "pxor xmm0, xmm1",
            expand!("0x01"),
            "aesenc xmm0, xmm1",
            expand!("0x02"),
            "aesenc xmm0, xmm1",
            expand!("0x04"),
            "aesenc xmm0, xmm1",
            expand!("0x08"),
            "aesenc xmm0, xmm1",
            expand!("0x10"),
            "aesenc xmm0, xmm1",
            expand!("0x20"),
            "aesenc xmm0, xmm1",
            expand!("0x40"),
            "aesenc xmm0, xmm1",
            expand!("0x80"),
            "aesenc xmm0, xmm1",
            expand!("0x1b"),
            "aesenc xmm0, xmm1",
            expand!("0x36"),
            "aesenclast xmm0, xmm1",
            "movdqu xmmword ptr [{block}], xmm0",
            "pxor xmm0, xmm0",
            "pxor xmm1, xmm1",
            "pxor xmm2, xmm2",
            "pxor xmm3, xmm3",
            block = in(reg) block.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

/// Put a known key in `xmm15`, for the self-test.
///
/// # Safety
/// As [`encrypt_block`], minus the key.
pub unsafe fn load_key(key: &[u8; 16]) {
    // SAFETY: forwarded from the caller; `key` is 16 readable bytes.
    unsafe {
        core::arch::asm!(
            "movdqu xmm15, xmmword ptr [{key}]",
            key = in(reg) key.as_ptr(),
            options(nostack, preserves_flags, readonly),
        );
    }
}

/// Destroy the key: zero `xmm15`.
///
/// # Safety
/// SSE must be enabled on this CPU.
pub unsafe fn forget_key() {
    // SAFETY: forwarded from the caller; `pxor` touches nothing but xmm15.
    unsafe {
        core::arch::asm!(
            "pxor xmm15, xmm15",
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Fill `xmm15` with a fresh key from RDRAND. Returns false, with `xmm15`
/// zeroed, if RDRAND would not produce one.
///
/// Each half gets up to 100 attempts. All-ones counts as a failure even when
/// RDRAND reports success: some AMD parts returned exactly that after resume,
/// until a microcode fix. So does a second half equal to the first, which is
/// what a generator stuck on any other value would hand back.
///
/// # Safety
/// As [`encrypt_block`], minus the key.
pub unsafe fn generate_key() -> bool {
    let ok: u32;
    // SAFETY: forwarded from the caller. rax, rcx and rdx are declared
    // clobbered; rax carries each random half into xmm15, rdx remembers the
    // first to compare the second against, and both are cleared before the end.
    unsafe {
        core::arch::asm!(
            "mov ecx, 100",
            "2:",
            "rdrand rax",
            "jnc 5f",
            "cmp rax, -1",
            "jne 3f",
            "5:",
            "dec ecx",
            "jnz 2b",
            "jmp 9f",
            "3:",
            "movq xmm15, rax",
            "mov rdx, rax",
            "mov ecx, 100",
            "4:",
            "rdrand rax",
            "jnc 6f",
            "cmp rax, -1",
            "je 6f",
            "cmp rax, rdx",
            "jne 7f",
            "6:",
            "dec ecx",
            "jnz 4b",
            "jmp 9f",
            "7:",
            "pinsrq xmm15, rax, 1",
            "mov {ok:e}, 1",
            "jmp 8f",
            "9:",
            "pxor xmm15, xmm15",
            "mov {ok:e}, 0",
            "8:",
            "xor eax, eax",
            "xor edx, edx",
            ok = out(reg) ok,
            out("rax") _,
            out("rcx") _,
            out("rdx") _,
            options(nomem, nostack),
        );
    }
    ok == 1
}

/// FIPS-197 appendix C.1: AES-128 on a known key and block.
///
/// # Safety
/// As [`encrypt_block`], minus the key. Overwrites `xmm15`, so run it before
/// [`generate_key`], never after.
pub unsafe fn self_test() -> bool {
    const KEY: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const PLAIN: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const CIPHER: [u8; 16] = [
        0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5,
        0x5a,
    ];
    let mut block = PLAIN;
    // SAFETY: forwarded from the caller.
    unsafe {
        load_key(&KEY);
        encrypt_block(&mut block);
    }
    block == CIPHER
}
