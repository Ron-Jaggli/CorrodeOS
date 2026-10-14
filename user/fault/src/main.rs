//! A program that goes wrong on purpose.
//!
//! It reads a kernel address. The page is mapped and readable -- from ring
//! 0 -- so from here the read must fault, and the fault must stop this
//! program and nothing else. The kernel checks both halves: that this task
//! ended, and that the machine did not.
//!
//! This is what a crashing driver looks like from the kernel's side.

#![no_std]
#![no_main]

corrode_rt::entry!(main);

/// The kernel image's first byte: the top two gigabytes of the address space.
const KERNEL: usize = 0xffff_ffff_8000_0000;

fn main() {
    // SAFETY: none. The address is not ours and the read is expected to
    // trap; that is the entire program.
    let byte = unsafe { core::ptr::read_volatile(KERNEL as *const u8) };
    // Not reached. If it is, say so in the only way a task with no
    // capabilities can: by still being here to be counted.
    core::hint::black_box(byte);
    loop {
        corrode_rt::yield_now();
    }
}
