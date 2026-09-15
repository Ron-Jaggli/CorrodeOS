//! Bringing up the other CPUs.
//!
//! Limine has already taken every application processor out of reset, through
//! real mode, and into 64-bit long mode, where it parks each one spinning on a
//! `goto_address` field. Writing a function pointer there starts it. That is
//! worth not reimplementing: the alternative is an INIT-SIPI-SIPI sequence and
//! a hand-written real-mode trampoline copied below 1 MiB, which is a lot of
//! fragile code to own for no gain over what the bootloader already did.
//!
//! # What an AP arrives with, and what it does not
//!
//! It arrives on the *bootloader's* page tables and stack, with interrupts off.
//! Almost nothing about the kernel's environment is set up for it, and several
//! of the things that look global are per-CPU registers that have to be set
//! again on every core:
//!
//! * `CR3` -- the bootloader's tables, not ours;
//! * `GDTR`, `IDTR`, `TR` -- each CPU loads its own, and its own TSS;
//! * `EFER.NXE` and `CR0.WP` -- per-CPU. Miss `NXE` and bit 63 of a page table
//!   entry goes back to being reserved, so every NX page faults on that core
//!   and nowhere else. Miss `CR0.WP` and W^X is decorative on that core alone.
//! * the local APIC's enable bit, task priority and spurious vector.
//!
//! The order matters. The kernel stack we allocated for an AP is in the kernel
//! half of *our* address space, which the bootloader's tables do not have, so
//! CR3 has to be loaded before the stack can be switched. That is why entry is
//! a naked function: those two steps have to happen before any compiled code
//! that might touch a stack.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::x86_64::{self, cpu, gdt, idt, lapic, vectors};
use crate::limine;
use crate::mm::paging;
use crate::task;

/// What an AP needs before it can run kernel code, reachable from both address
/// spaces because it lives in the kernel image.
#[repr(C)]
struct ApStart {
    /// Our PML4. Loaded first: everything else here is only mapped afterwards.
    cr3: u64,
    /// Top of the kernel stack allocated for this CPU.
    stack_top: u64,
    /// Dense CPU index, passed to [`ap_main`].
    cpu: u64,
}

const NO_START: ApStart = ApStart {
    cr3: 0,
    stack_top: 0,
    cpu: 0,
};

static mut AP_START: [ApStart; cpu::MAX_CPUS] = [NO_START; cpu::MAX_CPUS];

/// CPUs that have finished [`ap_main`]'s setup and are taking interrupts.
static ONLINE: AtomicUsize = AtomicUsize::new(1);

/// How many CPUs are running kernel code, the bootstrap processor included.
pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// First instructions an application processor executes in this kernel.
///
/// Naked because the first two things it does cannot be expressed in Rust:
/// install our address space, and only then move to a stack that address space
/// contains. Between those two instructions there is no usable stack, so
/// nothing may be pushed, called or spilled.
#[unsafe(naked)]
extern "C" fn ap_trampoline(info: *const limine::SmpInfo) -> ! {
    core::arch::naked_asm!(
        // rdi = our limine_smp_info. Offset 24 is extra_argument, where
        // `start_aps` left a pointer to our ApStart.
        "mov rax, [rdi + 24]",
        // Install our page tables. RIP stays valid across this because the
        // kernel image is mapped in both, and so is the ApStart we are reading.
        "mov rcx, [rax]",
        "mov cr3, rcx",
        // Only now is this stack mapped.
        "mov rsp, [rax + 8]",
        // Terminate the frame-pointer chain: nothing sensible is below us.
        "xor rbp, rbp",
        "mov rdi, [rax + 16]",
        "call {main}",
        // `ap_main` does not return.
        "2:",
        "hlt",
        "jmp 2b",
        main = sym ap_main,
    )
}

/// Everything an AP needs once it has a stack, in the order it needs it.
extern "C" fn ap_main(cpu: usize) -> ! {
    // Per-CPU control registers first: page table entries with the NX bit set
    // are already installed, and this core would fault on them otherwise.
    //
    // SAFETY: the tables using NX exist and CR0.WP only tightens ring 0 write
    // checks. Both were already true on the bootstrap processor; this is the
    // same settings applied to this core.
    unsafe { x86_64::enable_memory_protections() };

    gdt::load(cpu);
    idt::load();
    lapic::enable_local();

    // SAFETY: only this CPU reads or writes its own entry, and it does so
    // before any other CPU could look at it.
    let starts = &raw const AP_START;
    let stack_top = unsafe { (*starts)[cpu].stack_top };

    // Become a task, so there is something for the scheduler to switch away
    // from when the first tick lands. This context is a halt loop, which is
    // exactly what an idle task is, so it becomes this CPU's.
    task::adopt_as_idle(cpu, stack_top);

    lapic::start_timer(vectors::LAPIC_TIMER, crate::TICK_HZ);

    ONLINE.fetch_add(1, Ordering::Release);

    // SAFETY: this CPU has an IDT, a TSS, an enabled local APIC, and a handler
    // for every vector it has unmasked.
    unsafe { x86_64::enable_interrupts() };

    loop {
        x86_64::halt();
    }
}

/// Start every application processor the firmware reported.
///
/// Returns `(online, reported)`.
pub fn start_aps() -> (usize, usize) {
    let Some(response) = limine::SMP.response.get() else {
        crate::println!("smp        : no response -- single processor");
        return (1, 1);
    };

    // The bootstrap processor takes index 0, claimed before any AP, so the
    // index it has been using since boot stays correct.
    cpu::register(response.bsp_lapic_id);

    let reported = response.cpu_count as usize;
    let mut launched = 0;

    for info in response.cpus() {
        if info.lapic_id == response.bsp_lapic_id {
            continue;
        }

        let Some(index) = cpu::register(info.lapic_id) else {
            crate::println!(
                "smp        : lapic {} left parked (over the {} CPU limit)",
                info.lapic_id,
                cpu::MAX_CPUS
            );
            continue;
        };

        let Some(stack) = task::stack::allocate() else {
            crate::println!(
                "smp        : lapic {} left parked (no stack)",
                info.lapic_id
            );
            continue;
        };

        let Some(cr3) = paging::kernel_root() else {
            return (1, reported);
        };

        // SAFETY: this CPU has not been started yet, so nothing else reads or
        // writes its slot. The write must be complete before `goto_address` is
        // set, which the release store below orders.
        unsafe {
            let starts = &raw mut AP_START;
            (*starts)[index] = ApStart {
                cr3,
                stack_top: stack.top,
                cpu: index as u64,
            };
        }

        // SAFETY: taking the address of one element of a static. The pointer
        // is handed to a CPU that has not started yet, and only that CPU reads
        // it, so there is no aliasing to claim.
        let slot = unsafe { (&raw const AP_START).cast::<ApStart>().add(index) };
        info.extra_argument.store(slot as u64, Ordering::Relaxed);

        // The release store is what starts the CPU. Everything above has to be
        // visible to it first, which is exactly what release ordering buys.
        info.goto_address
            .store(ap_trampoline as *const () as u64, Ordering::Release);

        launched += 1;
    }

    wait_for(launched + 1);
    (online(), reported)
}

/// Spin until every launched CPU has checked in, or give up.
///
/// Bounded because an AP that never arrives must not take the boot with it. A
/// CPU that fails to come up is a reason to run with fewer, not a reason to
/// hang before the first line of output about it.
fn wait_for(expected: usize) {
    const ATTEMPTS: u64 = 200_000_000;
    let mut spins = 0u64;
    while online() < expected && spins < ATTEMPTS {
        core::hint::spin_loop();
        spins += 1;
    }
}
