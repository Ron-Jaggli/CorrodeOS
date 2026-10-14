/// Interrupt vectors we assign ourselves.
///
/// Kept well clear of the 32 CPU exceptions and of the legacy PIC's 0x20-0x2F,
/// so a stray legacy interrupt can never be mistaken for an APIC one.
pub mod vectors {
    /// Local APIC timer -- the scheduling tick.
    pub const LAPIC_TIMER: u8 = 0x40;
    /// The PIT, routed through the I/O APIC.
    pub const IOAPIC_TIMER: u8 = 0x41;
    /// Voluntary reschedule, raised by software with `int`.
    pub const YIELD: u8 = 0x42;
    /// Sent to every other CPU when this one has decided the machine is over.
    pub const HALT: u8 = 0x43;
    /// Device interrupts: one vector for each line [`crate::irq`] hands out.
    /// Sixteen vectors is one priority class, which the LAPIC does not care
    /// about while the task priority register is zero.
    pub const DEVICE_BASE: u8 = 0x50;
    pub const DEVICE_END: u8 = 0x70;
    /// The system call gate, and the only IDT entry reachable from ring 3.
    ///
    /// Deliberately an ordinary trap vector rather than `syscall`/`sysret`.
    /// The stubs already normalise every entry into one `TrapFrame`, which is
    /// also what a context switch saves and restores -- so a syscall that
    /// blocks is just a frame the scheduler hands to someone else, with no
    /// second entry path to keep in sync. Fast syscalls are an optimisation to
    /// make later, against a mechanism that already works.
    pub const SYSCALL: u8 = 0x80;
    /// Raised by the LAPIC when an interrupt is withdrawn before delivery.
    /// Must never be acknowledged.
    pub const SPURIOUS: u8 = 0xff;
}

pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod lapic;
pub mod pic;
pub mod pit;
pub mod port;

/// Unmask interrupts on this CPU.
///
/// # Safety
/// The IDT must be installed and every unmasked source must have a handler,
/// otherwise the first interrupt is an unhandled trap.
pub unsafe fn enable_interrupts() {
    // SAFETY: caller guarantees handlers are in place; `sti` only sets IF.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Forget what this CPU has cached about one page's mapping.
///
/// Needed after changing an entry that was already present. A new mapping
/// where there was none needs nothing: there is no stale entry to evict.
/// Other CPUs are not told, which is right only for an address space that is
/// installed on this one alone -- true of every user space while tasks do not
/// migrate.
pub fn flush_page(va: u64) {
    // SAFETY: `invlpg` drops a TLB entry and touches no memory. At worst the
    // next access to that page walks the tables again.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
    }
}

/// Halt until the next interrupt arrives.
///
/// Requires interrupts to be enabled -- with IF clear this parks the CPU
/// permanently.
pub fn halt() {
    // SAFETY: `hlt` only stops the CPU until the next interrupt; it touches no
    // memory the compiler tracks.
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// Raise a breakpoint trap.
///
/// Used at boot to prove the whole interrupt path works: if the stubs save and
/// restore state correctly, execution continues on the next line as if nothing
/// happened. If they don't, it's obvious immediately rather than three
/// subsystems later.
pub fn breakpoint() {
    // SAFETY: `int3` raises vector 3, for which we install a resumable handler
    // in `idt`. It touches no memory the compiler tracks.
    unsafe {
        core::arch::asm!("int3", options(nomem, nostack));
    }
}

/// Halt this CPU forever, with interrupts masked.
///
/// Used by the panic handler and by `kmain` once it runs out of work. `hlt` in
/// a loop parks the core at near-zero power instead of spinning hot, which also
/// keeps QEMU's host CPU usage honest.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` only affect this CPU's interrupt flag and
        // halt state. We never intend to leave this loop, so masking interrupts
        // permanently is the desired outcome rather than a hazard.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

const IA32_EFER: u32 = 0xc000_0080;
const EFER_NXE: u64 = 1 << 11;
const CR0_WP: u64 = 1 << 16;

/// # Safety
/// `msr` must be a model-specific register that exists on this CPU.
unsafe fn read_msr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: caller guarantees the MSR exists; reading one is side-effect free
    // for the registers we use it on.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// # Safety
/// `msr` must exist and `value` must be a legal setting for it; a bad write
/// can fault or change CPU behaviour globally.
unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: caller guarantees the register and value are valid.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Turn on the two protections our page tables rely on.
///
/// * `EFER.NXE` makes bit 63 of a page table entry mean "no execute". Without
///   it that bit is reserved, and setting it faults instead of protecting
///   anything.
/// * `CR0.WP` makes ring 0 honour read-only pages. Without it the kernel can
///   happily write its own `.text` and W^X is decorative.
///
/// Limine may already have set both; setting them again is harmless.
///
/// # Safety
/// Must run before any page table entry using the NX bit is installed.
pub unsafe fn enable_memory_protections() {
    // SAFETY: IA32_EFER exists on every x86_64 CPU, and we only set a defined
    // feature bit in it.
    unsafe {
        let efer = read_msr(IA32_EFER);
        write_msr(IA32_EFER, efer | EFER_NXE);
    }

    // SAFETY: setting CR0.WP only tightens access checks for ring 0 writes.
    unsafe {
        let cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov cr0, {}", in(reg) cr0 | CR0_WP, options(nomem, nostack, preserves_flags));
    }
}

/// Run `f` with interrupts disabled, restoring the previous state after.
///
/// Needed wherever a lock is shared with an interrupt handler. Taking such a
/// lock with interrupts enabled invites the handler to fire mid-critical-section
/// and spin forever on a lock its own interruptee holds -- a deadlock that only
/// shows up under load.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let flags: u64;
    // SAFETY: reads RFLAGS via the stack and clears IF. `pushfq` touches the
    // stack, so this deliberately does not claim `nostack`.
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) flags);
        core::arch::asm!("cli", options(nomem, nostack));
    }

    let result = f();

    // Bit 9 is IF. Only re-enable if it was set: blindly running `sti` would
    // turn interrupts on inside a handler that entered with them off.
    if flags & (1 << 9) != 0 {
        // SAFETY: restoring the caller's own interrupt state.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }

    result
}
