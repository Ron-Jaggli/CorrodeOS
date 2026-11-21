//! Each task's x87 and SSE registers, saved while it is not running.
//!
//! The kernel and the programs are built soft-float, so the compiler never
//! puts anything in an XMM register. SSE is used only by hand, inside single
//! `asm!` blocks in the renderer's pixel loops, and no such block makes a
//! call, takes a lock or crosses into the kernel. So the one place a task's
//! XMM registers hold something another task could disturb is the middle of
//! one of those blocks, and the only thing that can run another task's block
//! there is a task switch. An interrupt handler runs no SSE of its own, which
//! is why the trap stubs still save general-purpose registers only.
//!
//! So the switch saves the outgoing task's registers and loads the incoming
//! one's, every time, rather than lazily on first use: a lazy switch leaves
//! one task's registers in the CPU while another runs, which is a leak
//! waiting for a speculative read (CVE-2018-3665).
//!
//! The secure core never switches tasks, so its `xmm15` is never saved here.

/// The `fxsave` image: 512 bytes, 16-aligned.
#[repr(C, align(16))]
pub struct FpuState([u8; 512]);

impl FpuState {
    /// The state `fninit` and a reset MXCSR leave: every exception masked,
    /// round to nearest, every register zero.
    pub const INITIAL: Self = {
        let mut bytes = [0u8; 512];
        // FCW, the x87 control word, at offset 0.
        bytes[0] = 0x7f;
        bytes[1] = 0x03;
        // MXCSR at offset 24.
        bytes[24] = 0x80;
        bytes[25] = 0x1f;
        Self(bytes)
    };
}

/// Save this CPU's registers into `from`, and load `to`'s.
///
/// # Safety
/// SSE must be on on this CPU (see [`super::enable_sse`]), and `to` must hold
/// a state that `fxsave` wrote or [`FpuState::INITIAL`]: reserved MXCSR bits
/// set there fault.
pub unsafe fn switch(from: &mut FpuState, to: &FpuState) {
    // SAFETY: both images are 512 bytes and 16-aligned by their type.
    unsafe {
        core::arch::asm!(
            "fxsave64 [{from}]",
            "fxrstor64 [{to}]",
            from = in(reg) from.0.as_mut_ptr(),
            to = in(reg) to.0.as_ptr(),
            options(nostack, preserves_flags),
        );
    }
}
