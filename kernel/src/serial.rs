//! 16550 UART on COM1 -- the kernel's console until there's anything better.
//!
//! QEMU is launched with `-serial stdio`, so everything written here lands in
//! the terminal that started the VM. This is the primary debugging channel for
//! boot, paging and interrupt work.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::x86_64;
use crate::arch::x86_64::port::{inb, outb};

const COM1: u16 = 0x3f8;

// Register offsets from the port base.
const DATA: u16 = 0;
const INT_ENABLE: u16 = 1;
const FIFO_CTRL: u16 = 2;
const LINE_CTRL: u16 = 3;
const MODEM_CTRL: u16 = 4;
const LINE_STATUS: u16 = 5;

const LSR_THR_EMPTY: u8 = 1 << 5;

pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    /// Configure the UART for 38400 baud, 8N1, FIFOs on.
    fn init(base: u16) -> Self {
        // SAFETY: COM1 at 0x3f8 is a fixed, architecturally-defined ISA port
        // range on x86. This is the standard 16550 init sequence; each write
        // targets a register of that UART and no other device. If no UART is
        // present the writes are discarded harmlessly.
        unsafe {
            outb(base + INT_ENABLE, 0x00); // mask all UART interrupts
            outb(base + LINE_CTRL, 0x80); // DLAB on: next two writes set divisor
            outb(base + DATA, 0x03); // divisor low  = 3 -> 38400 baud
            outb(base + INT_ENABLE, 0x00); // divisor high = 0
            outb(base + LINE_CTRL, 0x03); // DLAB off, 8 bits, no parity, 1 stop
            outb(base + FIFO_CTRL, 0xc7); // enable + clear FIFOs, 14-byte trigger
            outb(base + MODEM_CTRL, 0x0b); // DTR + RTS + OUT2
        }
        Self { base }
    }

    fn write_byte(&mut self, byte: u8) {
        // SAFETY: reading the line status register has no side effects, and the
        // data register only accepts a byte once THR-empty is set, which is
        // exactly what we spin on.
        unsafe {
            while inb(self.base + LINE_STATUS) & LSR_THR_EMPTY == 0 {
                core::hint::spin_loop();
            }
            outb(self.base + DATA, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Terminals want CRLF; the kernel only ever emits '\n'.
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// Dead-simple spinlock. There is exactly one CPU running at this stage, but
/// the console is the one thing guaranteed to be touched from a panic path, so
/// it gets a lock from day one rather than a retrofit later.
///
/// It is always taken with interrupts disabled -- see [`_print`] for why that
/// is load-bearing rather than tidiness.
struct SpinLock {
    locked: AtomicBool,
}

static LOCK: SpinLock = SpinLock {
    locked: AtomicBool::new(false),
};

static mut PORT: Option<SerialPort> = None;

/// Bring up COM1. Call once, early, before anything wants to print.
pub fn init() {
    let port = SerialPort::init(COM1);
    // SAFETY: called exactly once from `kmain` before any other CPU is started
    // and before any print can occur, so there is no concurrent access to
    // `PORT` at this moment.
    unsafe {
        PORT = Some(port);
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // Interrupts off for the whole print, not just for tidiness of the output.
    //
    // The console lock is taken from preemptible task code *and* from code that
    // already holds the scheduler lock with interrupts disabled (`task::spawn`
    // logs the stack it just mapped). If a task could be preempted holding this
    // lock, the next context to take it with interrupts off would spin on a
    // holder that can never be rescheduled -- a hard hang, mid-line, nowhere
    // near the cause. Holding it only across an uninterruptible region makes
    // that unrepresentable: nobody is ever descheduled owning the console.
    //
    // The cost is real but bounded: a print is a few dozen bytes at 38400 baud
    // with interrupts masked. Sharing one UART means somebody waits either way.
    x86_64::without_interrupts(|| {
        while LOCK
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        // SAFETY: the spinlock above grants exclusive access to `PORT` for the
        // duration of this block, so the `&mut` is unique.
        unsafe {
            let port = &mut *core::ptr::addr_of_mut!(PORT);
            if let Some(port) = port.as_mut() {
                let _ = port.write_fmt(args);
            }
        }

        LOCK.locked.store(false, Ordering::Release);
    });
}

/// Force-release the console lock. Only for the panic path, where the panicking
/// CPU may itself be the lock holder and we would otherwise deadlock printing
/// the panic message.
///
/// # Safety
/// Caller must be on a path that never returns to normal execution.
pub unsafe fn force_unlock() {
    LOCK.locked.store(false, Ordering::Release);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
