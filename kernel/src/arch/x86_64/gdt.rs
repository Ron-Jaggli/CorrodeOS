//! Global Descriptor Table and Task State Segment.
//!
//! Limine hands us a working GDT, but it belongs to the bootloader and its
//! memory is reclaimable. We install our own before touching interrupts,
//! because every IDT entry names a code selector and that selector has to stay
//! valid for the life of the kernel.
//!
//! The TSS matters for one reason right now: IST1. A stack-overflow #PF that
//! escalates to #DF while RSP is already garbage will triple-fault and reboot
//! the machine with no diagnostics. IST1 gives #DF a known-good stack so it can
//! actually print something.

use core::arch::asm;
use core::mem::size_of;
use core::ptr::addr_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
/// Ring 3 selectors. The low two bits are the requested privilege level, and
/// they are part of the selector a user task actually runs with: `iretq` into a
/// CS whose RPL is 3 is what drops the CPU to ring 3 in the first place.
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS_SELECTOR: u16 = 0x28;

/// IST slot used by the double-fault handler. 1-based, as the CPU numbers them.
pub const IST_DOUBLE_FAULT: u8 = 1;

// Flat 64-bit descriptors. Base and limit are ignored in long mode for code and
// data segments; these are the canonical encodings.
const NULL: u64 = 0;
const KCODE: u64 = 0x00af_9a00_0000_ffff;
const KDATA: u64 = 0x00cf_9200_0000_ffff;
const UDATA: u64 = 0x00cf_f200_0000_ffff;
const UCODE: u64 = 0x00af_fa00_0000_ffff;

/// Backing stack for double faults. Never touched unless things have already
/// gone badly wrong.
const DF_STACK_SIZE: usize = 16 * 1024;
static mut DF_STACK: [u8; DF_STACK_SIZE] = [0; DF_STACK_SIZE];

#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    /// Stack pointers for privilege levels 0-2. rsp[0] is what the CPU switches
    /// to on a ring 3 -> ring 0 transition, kept current by the scheduler via
    /// [`set_kernel_stack`].
    rsp: [u64; 3],
    reserved1: u64,
    /// Interrupt Stack Table. Entry n-1 here is what IST value n selects.
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    // No I/O permission bitmap: point past the end of the TSS so the CPU treats
    // every port as denied for ring 3.
    iomap_base: size_of::<Tss>() as u16,
};

/// null, kcode, kdata, udata, ucode, then the TSS descriptor (which is 16 bytes,
/// so it occupies two slots).
///
/// NOTE: this ordering is chosen for clarity, not for SYSRET. When fast syscalls
/// go in, SYSRET's fixed CS/SS arithmetic off STAR[63:48] will pin the user
/// selectors to a specific order and this table will need revisiting.
static mut GDT: [u64; 7] = [NULL, KCODE, KDATA, UDATA, UCODE, 0, 0];

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build the 16-byte TSS system descriptor for a given TSS address.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<Tss>() - 1) as u64;

    let low = (limit & 0xffff)
        | ((base & 0x00ff_ffff) << 16)
        // present, DPL 0, type 0x9 = available 64-bit TSS
        | (0x89u64 << 40)
        | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);

    let high = (base >> 32) & 0xffff_ffff;

    (low, high)
}

/// Point the TSS at the kernel stack to use for the next ring 3 -> ring 0 trap.
///
/// The scheduler calls this with the incoming task's own kernel stack, so a
/// trap from ring 3 lands on a stack belonging to the task that took it rather
/// than on whichever one happened to be there. It is only ever consulted on a
/// privilege change, and a task in ring 3 has by definition unwound its kernel
/// stack, so the top is always the right value.
pub fn set_kernel_stack(top: u64) {
    // SAFETY: the TSS is a single static the CPU reads on privilege changes.
    // This is the only writer of rsp[0], and it runs with interrupts disabled
    // inside the scheduler, so no trap can observe a half-written pointer --
    // and a u64 store is single-instruction regardless.
    unsafe {
        // Via a raw pointer rather than a reference: `&mut TSS` on a mutable
        // static is exactly the aliasing claim we cannot make, since the CPU
        // reads this structure behind our back on every privilege change.
        let tss = &raw mut TSS;
        (*tss).rsp[0] = top;
    }
}

/// Install the GDT, load the TSS, and reload every segment register.
///
/// Call once, early, before `idt::init`.
pub fn init() {
    // SAFETY: single-threaded early boot, called exactly once. These statics are
    // not reachable from anywhere else at this point, so taking raw pointers to
    // them and writing through them is unaliased.
    unsafe {
        let df_top = addr_of!(DF_STACK) as u64 + DF_STACK_SIZE as u64;
        // Stacks grow down and the CPU requires 16-byte alignment on entry.
        let tss = &raw mut TSS;
        (*tss).ist[(IST_DOUBLE_FAULT - 1) as usize] = df_top & !0xf;

        let (low, high) = tss_descriptor(tss as u64);
        let gdt = &raw mut GDT;
        (*gdt)[5] = low;
        (*gdt)[6] = high;

        let gdtr = DescriptorTablePointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: gdt as u64,
        };

        load_gdt(&gdtr);
        load_tss(TSS_SELECTOR);
    }
}

/// # Safety
/// `gdtr` must describe a well-formed GDT whose slot 1 is a valid 64-bit code
/// segment and slot 2 a valid data segment, since this reloads CS and the data
/// registers to those selectors.
unsafe fn load_gdt(gdtr: &DescriptorTablePointer) {
    // SAFETY: `lgdt` reads the pointer we just built. CS cannot be loaded with a
    // plain mov, so it is reloaded with a far return: push the target selector
    // and address, then `lretq` pops them into CS:RIP. The `2:` label is the
    // instruction immediately after, so control resumes exactly where it would
    // have anyway, just with the new CS. The data selectors are plain movs.
    unsafe {
        asm!(
            "lgdt [{gdtr}]",
            "push {kcode}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {kdata:x}",
            "mov es, {kdata:x}",
            "mov ss, {kdata:x}",
            "mov fs, {kdata:x}",
            "mov gs, {kdata:x}",
            gdtr = in(reg) gdtr,
            kcode = const KERNEL_CODE as u64,
            kdata = in(reg) KERNEL_DATA,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// # Safety
/// `selector` must index a present, available 64-bit TSS descriptor in the
/// currently loaded GDT.
unsafe fn load_tss(selector: u16) {
    // SAFETY: caller guarantees the selector names a valid TSS descriptor; `ltr`
    // marks it busy and loads the task register. No Rust-visible memory changes.
    unsafe {
        asm!("ltr {0:x}", in(reg) selector, options(nomem, nostack, preserves_flags));
    }
}
