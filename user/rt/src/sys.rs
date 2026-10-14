//! The system call instruction.
//!
//! One function, so the register convention is written down once on this
//! side of the boundary. See `corrode-abi` for what the registers mean.

use core::arch::asm;

/// What goes in.
#[derive(Clone, Copy)]
pub struct Args {
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    /// A capability to send, and the mask to narrow it by.
    pub r12: u64,
    pub r13: u64,
    /// Where a capability in the answer to a call should land.
    pub r14: u64,
}

impl Args {
    /// Nothing in any register, and no capability riding along. A call that
    /// forgets to say "no capability" would otherwise offer up whatever is
    /// in slot zero.
    pub const NONE: Args = Args {
        rdi: 0,
        rsi: 0,
        rdx: 0,
        r10: 0,
        r8: 0,
        r9: 0,
        r12: corrode_abi::NO_SLOT,
        r13: 0,
        r14: corrode_abi::NO_SLOT,
    };
}

/// What comes back.
#[derive(Clone, Copy)]
pub struct Out {
    /// Zero, or why not.
    pub rax: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
}

/// Enter the kernel.
///
/// Safe to call with any numbers at all. That is a property of the kernel,
/// not of this function: nothing passed here is a pointer the kernel will
/// follow, so there is no argument that makes it touch this program's
/// memory. The worst a wrong number does is come back as an error.
#[inline]
pub fn call(number: u64, args: Args) -> Out {
    let (rax, rdi, rsi, rdx, r10, r8, r9);
    // SAFETY: `int 0x80` is the one gate ring 3 may enter. The kernel saves
    // and restores every register it does not return a value in, including
    // the flags, and takes the trap on its own stack rather than ours.
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number => rax,
            inlateout("rdi") args.rdi => rdi,
            inlateout("rsi") args.rsi => rsi,
            inlateout("rdx") args.rdx => rdx,
            inlateout("r10") args.r10 => r10,
            inlateout("r8") args.r8 => r8,
            inlateout("r9") args.r9 => r9,
            in("r12") args.r12,
            in("r13") args.r13,
            in("r14") args.r14,
            options(nostack, preserves_flags),
        );
    }
    Out {
        rax,
        rdi,
        rsi,
        rdx,
        r10,
        r8,
        r9,
    }
}
