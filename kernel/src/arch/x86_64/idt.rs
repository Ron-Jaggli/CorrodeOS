//! Interrupt Descriptor Table and trap entry.
//!
//! The `x86-interrupt` calling convention is still nightly-only, so every entry
//! point here is a hand-written naked stub. That's not a workaround we're
//! tolerating -- it's the design we want. The stubs normalise every vector into
//! one `TrapFrame` layout, which is the same structure a context switch will
//! need to save and restore, so scheduling gets to reuse it rather than invent
//! a parallel one.
//!
//! Entry path for every vector:
//!
//! ```text
//!   CPU        pushes ss, rsp, rflags, cs, rip   (+ error code on some vectors)
//!   stub       pushes a dummy 0 if the CPU didn't push an error code
//!   stub       pushes the vector number
//!   common     pushes all 15 GPRs, calls trap_dispatch(&mut TrapFrame)
//!   common     pops GPRs, drops vector+error code, iretq
//! ```

use core::arch::{asm, naked_asm};
use core::mem::size_of;
use core::ptr::addr_of;

use super::gdt;
use super::halt_forever;
use super::{lapic, pic, pit, vectors};

/// A stub for every vector. The APIC needs handlers well above the legacy PIC
/// range -- its spurious vector alone is 0xFF -- and a present entry that
/// reports an unexpected interrupt beats a not-present one that turns it into a
/// #GP with the original vector lost.
const VECTOR_COUNT: usize = 256;
const IDT_LEN: usize = 256;

/// Saved machine state at the point of a trap.
///
/// Field order is memory order, lowest address first, and it must stay exactly
/// in sync with the push/pop sequence in `common_stub`. Reordering a field here
/// without reordering the assembly corrupts every register on return.
#[derive(Debug)]
#[repr(C)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    /// Zero for vectors where the CPU doesn't push one.
    pub error_code: u64,
    // --- pushed by the CPU ---
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// Vectors where the CPU pushes an error code before the frame.
const fn has_error_code(vector: u64) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

/// Stub for vectors with no CPU-pushed error code: push a zero so every frame
/// has identical shape.
#[unsafe(naked)]
extern "C" fn isr_noerr<const V: u64>() {
    naked_asm!(
        "push 0",
        "push {vec}",
        "jmp {common}",
        vec = const V,
        common = sym common_stub,
    )
}

/// Stub for vectors where the CPU already pushed an error code.
#[unsafe(naked)]
extern "C" fn isr_err<const V: u64>() {
    naked_asm!(
        "push {vec}",
        "jmp {common}",
        vec = const V,
        common = sym common_stub,
    )
}

/// Save state, call into Rust, restore, return from interrupt.
///
/// Stack alignment: the CPU aligns RSP to 16 before pushing its 5 (or 6) words.
/// After normalisation we are 7 words deep, and 15 GPR pushes bring that to 22
/// words -- an even number, so RSP is 16-byte aligned at the `call`, which is
/// what the SysV ABI requires.
#[unsafe(naked)]
extern "C" fn common_stub() {
    naked_asm!(
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // The ABI says the direction flag is clear on entry to a C function and
        // an interrupt can land anywhere, so don't assume it.
        "cld",
        "mov rdi, rsp",
        "call {dispatch}",
        // The dispatcher returns the frame to resume, which is not necessarily
        // the one we saved: a context switch is exactly "return a different
        // stack pointer". Everything below then restores that task's state
        // instead of ours, and iretq reloads its RSP too (in 64-bit mode iretq
        // always pops SS:RSP, even without a privilege change).
        "mov rsp, rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        // Drop the vector and error code the stubs pushed.
        "add rsp, 16",
        "iretq",
        dispatch = sym trap_dispatch,
    )
}

/// Pick the right stub for a vector at compile time.
const fn handler_for<const V: u64>() -> extern "C" fn() {
    if has_error_code(V) {
        isr_err::<V>
    } else {
        isr_noerr::<V>
    }
}

/// The stub table, in vector order.
///
/// Written out rather than generated because const-generic instantiation can't
/// come from a loop, and an explicit table is easy to eyeball against the
/// vector numbering in the SDM.
static STUBS: [extern "C" fn(); VECTOR_COUNT] = [
    handler_for::<0>(),
    handler_for::<1>(),
    handler_for::<2>(),
    handler_for::<3>(),
    handler_for::<4>(),
    handler_for::<5>(),
    handler_for::<6>(),
    handler_for::<7>(),
    handler_for::<8>(),
    handler_for::<9>(),
    handler_for::<10>(),
    handler_for::<11>(),
    handler_for::<12>(),
    handler_for::<13>(),
    handler_for::<14>(),
    handler_for::<15>(),
    handler_for::<16>(),
    handler_for::<17>(),
    handler_for::<18>(),
    handler_for::<19>(),
    handler_for::<20>(),
    handler_for::<21>(),
    handler_for::<22>(),
    handler_for::<23>(),
    handler_for::<24>(),
    handler_for::<25>(),
    handler_for::<26>(),
    handler_for::<27>(),
    handler_for::<28>(),
    handler_for::<29>(),
    handler_for::<30>(),
    handler_for::<31>(),
    handler_for::<32>(),
    handler_for::<33>(),
    handler_for::<34>(),
    handler_for::<35>(),
    handler_for::<36>(),
    handler_for::<37>(),
    handler_for::<38>(),
    handler_for::<39>(),
    handler_for::<40>(),
    handler_for::<41>(),
    handler_for::<42>(),
    handler_for::<43>(),
    handler_for::<44>(),
    handler_for::<45>(),
    handler_for::<46>(),
    handler_for::<47>(),
    handler_for::<48>(),
    handler_for::<49>(),
    handler_for::<50>(),
    handler_for::<51>(),
    handler_for::<52>(),
    handler_for::<53>(),
    handler_for::<54>(),
    handler_for::<55>(),
    handler_for::<56>(),
    handler_for::<57>(),
    handler_for::<58>(),
    handler_for::<59>(),
    handler_for::<60>(),
    handler_for::<61>(),
    handler_for::<62>(),
    handler_for::<63>(),
    handler_for::<64>(),
    handler_for::<65>(),
    handler_for::<66>(),
    handler_for::<67>(),
    handler_for::<68>(),
    handler_for::<69>(),
    handler_for::<70>(),
    handler_for::<71>(),
    handler_for::<72>(),
    handler_for::<73>(),
    handler_for::<74>(),
    handler_for::<75>(),
    handler_for::<76>(),
    handler_for::<77>(),
    handler_for::<78>(),
    handler_for::<79>(),
    handler_for::<80>(),
    handler_for::<81>(),
    handler_for::<82>(),
    handler_for::<83>(),
    handler_for::<84>(),
    handler_for::<85>(),
    handler_for::<86>(),
    handler_for::<87>(),
    handler_for::<88>(),
    handler_for::<89>(),
    handler_for::<90>(),
    handler_for::<91>(),
    handler_for::<92>(),
    handler_for::<93>(),
    handler_for::<94>(),
    handler_for::<95>(),
    handler_for::<96>(),
    handler_for::<97>(),
    handler_for::<98>(),
    handler_for::<99>(),
    handler_for::<100>(),
    handler_for::<101>(),
    handler_for::<102>(),
    handler_for::<103>(),
    handler_for::<104>(),
    handler_for::<105>(),
    handler_for::<106>(),
    handler_for::<107>(),
    handler_for::<108>(),
    handler_for::<109>(),
    handler_for::<110>(),
    handler_for::<111>(),
    handler_for::<112>(),
    handler_for::<113>(),
    handler_for::<114>(),
    handler_for::<115>(),
    handler_for::<116>(),
    handler_for::<117>(),
    handler_for::<118>(),
    handler_for::<119>(),
    handler_for::<120>(),
    handler_for::<121>(),
    handler_for::<122>(),
    handler_for::<123>(),
    handler_for::<124>(),
    handler_for::<125>(),
    handler_for::<126>(),
    handler_for::<127>(),
    handler_for::<128>(),
    handler_for::<129>(),
    handler_for::<130>(),
    handler_for::<131>(),
    handler_for::<132>(),
    handler_for::<133>(),
    handler_for::<134>(),
    handler_for::<135>(),
    handler_for::<136>(),
    handler_for::<137>(),
    handler_for::<138>(),
    handler_for::<139>(),
    handler_for::<140>(),
    handler_for::<141>(),
    handler_for::<142>(),
    handler_for::<143>(),
    handler_for::<144>(),
    handler_for::<145>(),
    handler_for::<146>(),
    handler_for::<147>(),
    handler_for::<148>(),
    handler_for::<149>(),
    handler_for::<150>(),
    handler_for::<151>(),
    handler_for::<152>(),
    handler_for::<153>(),
    handler_for::<154>(),
    handler_for::<155>(),
    handler_for::<156>(),
    handler_for::<157>(),
    handler_for::<158>(),
    handler_for::<159>(),
    handler_for::<160>(),
    handler_for::<161>(),
    handler_for::<162>(),
    handler_for::<163>(),
    handler_for::<164>(),
    handler_for::<165>(),
    handler_for::<166>(),
    handler_for::<167>(),
    handler_for::<168>(),
    handler_for::<169>(),
    handler_for::<170>(),
    handler_for::<171>(),
    handler_for::<172>(),
    handler_for::<173>(),
    handler_for::<174>(),
    handler_for::<175>(),
    handler_for::<176>(),
    handler_for::<177>(),
    handler_for::<178>(),
    handler_for::<179>(),
    handler_for::<180>(),
    handler_for::<181>(),
    handler_for::<182>(),
    handler_for::<183>(),
    handler_for::<184>(),
    handler_for::<185>(),
    handler_for::<186>(),
    handler_for::<187>(),
    handler_for::<188>(),
    handler_for::<189>(),
    handler_for::<190>(),
    handler_for::<191>(),
    handler_for::<192>(),
    handler_for::<193>(),
    handler_for::<194>(),
    handler_for::<195>(),
    handler_for::<196>(),
    handler_for::<197>(),
    handler_for::<198>(),
    handler_for::<199>(),
    handler_for::<200>(),
    handler_for::<201>(),
    handler_for::<202>(),
    handler_for::<203>(),
    handler_for::<204>(),
    handler_for::<205>(),
    handler_for::<206>(),
    handler_for::<207>(),
    handler_for::<208>(),
    handler_for::<209>(),
    handler_for::<210>(),
    handler_for::<211>(),
    handler_for::<212>(),
    handler_for::<213>(),
    handler_for::<214>(),
    handler_for::<215>(),
    handler_for::<216>(),
    handler_for::<217>(),
    handler_for::<218>(),
    handler_for::<219>(),
    handler_for::<220>(),
    handler_for::<221>(),
    handler_for::<222>(),
    handler_for::<223>(),
    handler_for::<224>(),
    handler_for::<225>(),
    handler_for::<226>(),
    handler_for::<227>(),
    handler_for::<228>(),
    handler_for::<229>(),
    handler_for::<230>(),
    handler_for::<231>(),
    handler_for::<232>(),
    handler_for::<233>(),
    handler_for::<234>(),
    handler_for::<235>(),
    handler_for::<236>(),
    handler_for::<237>(),
    handler_for::<238>(),
    handler_for::<239>(),
    handler_for::<240>(),
    handler_for::<241>(),
    handler_for::<242>(),
    handler_for::<243>(),
    handler_for::<244>(),
    handler_for::<245>(),
    handler_for::<246>(),
    handler_for::<247>(),
    handler_for::<248>(),
    handler_for::<249>(),
    handler_for::<250>(),
    handler_for::<251>(),
    handler_for::<252>(),
    handler_for::<253>(),
    handler_for::<254>(),
    handler_for::<255>(),
];

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    fn set(&mut self, handler: extern "C" fn(), ist: u8, dpl: u8) {
        let addr = handler as usize as u64;
        self.offset_low = addr as u16;
        self.offset_mid = (addr >> 16) as u16;
        self.offset_high = (addr >> 32) as u32;
        self.selector = gdt::KERNEL_CODE;
        self.ist = ist & 0x7;
        // Present, 64-bit interrupt gate (clears IF on entry), DPL as given.
        //
        // DPL is the privilege needed to raise this vector with `int`, and it
        // is the whole ring 3 attack surface: every gate is DPL 0 except the
        // syscall vector, so a user task executing `int` on anything else
        // takes a #GP instead of entering a handler it was never meant to
        // reach. Hardware-delivered interrupts ignore DPL, so nothing is lost
        // by keeping it at 0 everywhere else.
        self.type_attr = 0x8e | ((dpl & 3) << 5);
        self.zero = 0;
    }
}

static mut IDT: [IdtEntry; IDT_LEN] = [IdtEntry::EMPTY; IDT_LEN];

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Install the IDT. `gdt::init` must have run first -- every gate names
/// `gdt::KERNEL_CODE`, and that selector has to resolve.
pub fn init() {
    // SAFETY: single-threaded early boot, called exactly once, and nothing else
    // holds a reference to IDT. Interrupts are still masked at this point, so no
    // handler can observe a half-built table.
    unsafe {
        let idt = &raw mut IDT;
        for (vector, stub) in STUBS.iter().enumerate() {
            // Double faults run on their own stack; if RSP is already corrupt,
            // reusing it here is what turns a #DF into a triple fault.
            let ist = if vector == 8 {
                gdt::IST_DOUBLE_FAULT
            } else {
                0
            };
            // The one gate ring 3 may enter deliberately.
            let dpl = if vector == vectors::SYSCALL as usize {
                3
            } else {
                0
            };
            (*idt)[vector].set(*stub, ist, dpl);
        }

        let idtr = DescriptorTablePointer {
            limit: (size_of::<[IdtEntry; IDT_LEN]>() - 1) as u16,
            base: addr_of!(IDT) as u64,
        };

        // SAFETY: `idtr` describes the table we just filled, and every present
        // entry points at a real stub with a valid code selector.
        asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags));
    }
}

const EXCEPTION_NAMES: [&str; 32] = [
    "divide error",
    "debug",
    "NMI",
    "breakpoint",
    "overflow",
    "bound range exceeded",
    "invalid opcode",
    "device not available",
    "double fault",
    "coprocessor segment overrun",
    "invalid TSS",
    "segment not present",
    "stack-segment fault",
    "general protection fault",
    "page fault",
    "reserved",
    "x87 floating-point",
    "alignment check",
    "machine check",
    "SIMD floating-point",
    "virtualization",
    "control protection",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "hypervisor injection",
    "VMM communication",
    "security exception",
    "reserved",
    "reserved",
];

fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: reading CR2 is side-effect free and always permitted in ring 0.
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Called by `common_stub` with a pointer to the saved state.
///
/// Returns the frame to resume from. Returning the frame it was given resumes
/// the interrupted code; returning another task's frame switches to that task.
extern "C" fn trap_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: `common_stub` passes the frame it just built on the current
    // stack; it is valid, uniquely owned for the duration of this call, and
    // correctly aligned.
    let frame_ref = unsafe { &mut *frame };
    let vector = frame_ref.vector;

    // Interrupts delivered through the local APIC. Ordinary events, not
    // faults, and all but one of them must be acknowledged.
    match vector as u8 {
        vectors::SPURIOUS => {
            // Deliberately no EOI. The LAPIC never set an in-service bit for a
            // spurious interrupt, so acknowledging one would clear the bit
            // belonging to a real interrupt still being serviced.
            return frame;
        }
        vectors::LAPIC_TIMER => {
            lapic::on_tick();
            // Acknowledge before switching: once we return another task's
            // frame we are not coming back here, and an unacknowledged LAPIC
            // never delivers this vector again.
            lapic::end_of_interrupt();
            return crate::task::on_tick(frame);
        }
        vectors::YIELD => {
            // Software interrupt, so there is nothing to acknowledge.
            return crate::task::on_tick(frame);
        }
        vectors::SYSCALL => {
            // Returns a frame because a syscall may block: the task that made
            // it can end up parked in IPC, and the frame handed back is then
            // some other task's. Same mechanism as a timer tick.
            return crate::syscall::dispatch(frame_ref);
        }
        vectors::IOAPIC_TIMER => {
            pit::on_tick();
            lapic::end_of_interrupt();
            return frame;
        }
        _ => {}
    }

    // Legacy 8259 range. Masked once the APIC is up, but a masked master can
    // still emit a spurious IRQ7, so this stays.
    let irq_base = pic::VECTOR_BASE as u64;
    if (irq_base..irq_base + 16).contains(&vector) {
        let irq = (vector - irq_base) as u8;
        // Only reachable on the fallback path, where the APIC was unavailable
        // and the PIT is still delivering through the PIC.
        if irq == pit::IRQ {
            pit::on_tick();
        }
        pic::end_of_interrupt(irq);
        return frame;
    }

    // Breakpoint is resumable: RIP already points past the int3.
    if vector == 3 {
        crate::println!(
            "trap       : breakpoint at {:#018x} (resuming)",
            frame_ref.rip
        );
        return frame;
    }

    let name = EXCEPTION_NAMES
        .get(vector as usize)
        .copied()
        .unwrap_or("interrupt");

    // This path never returns, so the console lock can be stolen. Without
    // this, faulting while kmain holds it would deadlock and we would lose the
    // one message that explains what happened.
    // SAFETY: `halt_forever` at the end of this function guarantees we never
    // resume the interrupted code, so no in-progress print can be corrupted.
    unsafe { crate::serial::force_unlock() };

    crate::println!();
    crate::println!("*** UNHANDLED TRAP ***");
    crate::println!("vector     : {} ({})", vector, name);
    crate::println!("error code : {:#x}", frame_ref.error_code);
    crate::println!(
        "rip        : {:#018x}  cs : {:#x}",
        frame_ref.rip,
        frame_ref.cs
    );
    crate::println!(
        "rsp        : {:#018x}  ss : {:#x}",
        frame_ref.rsp,
        frame_ref.ss
    );
    crate::println!("rflags     : {:#018x}", frame_ref.rflags);

    if vector == 8 {
        // A double fault almost always means the first fault could not be
        // delivered because the stack was unusable -- classically a stack
        // overflow that ran into a guard page, where pushing the #PF frame
        // faults in turn. CR2 still holds the address the original fault was
        // about, which is the single most useful number here.
        //
        // This handler only runs at all because its IDT entry uses IST1, giving
        // it a known-good stack. Without that this would be a triple fault: an
        // instant reset with nothing printed.
        crate::println!("cr2        : {:#018x}", read_cr2());
        crate::println!(
            "note       : rsp {:#018x} -- if that is just below a",
            frame_ref.rsp
        );
        crate::println!("             task stack, this is a stack overflow");
    }

    if vector == 14 {
        let cr2 = read_cr2();
        let ec = frame_ref.error_code;
        crate::println!("cr2        : {:#018x}", cr2);
        crate::println!(
            "cause      : {} on {} in {} mode{}",
            if ec & 1 == 0 {
                "not-present page"
            } else {
                "protection violation"
            },
            if ec & 2 == 0 { "read" } else { "write" },
            if ec & 4 == 0 { "kernel" } else { "user" },
            if ec & 16 != 0 {
                ", instruction fetch"
            } else {
                ""
            },
        );
    }

    crate::println!("rax {:#018x}  rbx {:#018x}", frame_ref.rax, frame_ref.rbx);
    crate::println!("rcx {:#018x}  rdx {:#018x}", frame_ref.rcx, frame_ref.rdx);
    crate::println!("rsi {:#018x}  rdi {:#018x}", frame_ref.rsi, frame_ref.rdi);
    crate::println!("rbp {:#018x}  r8  {:#018x}", frame_ref.rbp, frame_ref.r8);

    halt_forever();
}
