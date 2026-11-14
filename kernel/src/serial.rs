//! 16550 UART on COM1 -- the kernel's console until there's anything better.
//!
//! QEMU is launched with `-serial stdio`, so everything written here lands in
//! the terminal that started the VM. This is the primary debugging channel for
//! boot, paging and interrupt work.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Dead-simple spinlock, which records *which CPU* holds it.
///
/// It is always taken with interrupts disabled -- see [`_print`] for why that
/// is load-bearing rather than tidiness.
///
/// The owner is not bookkeeping. The panic and trap paths have to break this
/// lock, because the CPU taking the fault may be the one holding it, and
/// waiting for itself is a silent hang exactly when a message matters most.
/// Breaking it is only sound if the holder really is us: on more than one CPU,
/// stealing a lock another core is still using produces two writers on one UART
/// and output interleaved character by character.
struct SpinLock {
    locked: AtomicBool,
    owner: AtomicUsize,
}

/// No CPU holds the console.
const NO_OWNER: usize = usize::MAX;

static LOCK: SpinLock = SpinLock {
    locked: AtomicBool::new(false),
    owner: AtomicUsize::new(NO_OWNER),
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
        LOCK.owner.store(x86_64::cpu::index(), Ordering::Relaxed);

        // SAFETY: the spinlock above grants exclusive access to `PORT` for the
        // duration of this block, so the `&mut` is unique.
        unsafe {
            let port = &mut *core::ptr::addr_of_mut!(PORT);
            if let Some(port) = port.as_mut() {
                let _ = Tee(port).write_fmt(args);
            }
        }

        LOCK.owner.store(NO_OWNER, Ordering::Relaxed);
        LOCK.locked.store(false, Ordering::Release);
    });
}

/// COM1, and under `hw-log` the screen as well.
struct Tee<'a>(&'a mut SerialPort);

impl Write for Tee<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.write_str(s)?;
        // SAFETY: only constructed inside `_print`, with the console lock held.
        #[cfg(feature = "hw-log")]
        unsafe {
            crate::screen_log::write(s)
        };
        // SAFETY: as above.
        #[cfg(feature = "efi-log")]
        unsafe {
            crate::efi_log::write(s)
        };
        Ok(())
    }
}

/// Run `f` holding the console lock, so no line is printed halfway through it.
///
/// Waits a bounded time and then runs `f` regardless. Its callers include the
/// panic path, where the holder may be a CPU that has been halted mid-print;
/// a torn last line is a better outcome than never running `f`.
#[cfg(feature = "efi-log")]
pub fn with_console_lock<R>(f: impl FnOnce() -> R) -> R {
    x86_64::without_interrupts(|| {
        let mut held = false;
        for _ in 0..50_000_000u32 {
            if LOCK
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                held = true;
                break;
            }
            core::hint::spin_loop();
        }
        if held {
            LOCK.owner.store(x86_64::cpu::index(), Ordering::Relaxed);
        }
        let result = f();
        if held {
            LOCK.owner.store(NO_OWNER, Ordering::Relaxed);
            LOCK.locked.store(false, Ordering::Release);
        }
        result
    })
}

/// Redraw the on-screen log with the newest lines in order. Takes the console
/// lock the way `_print` does, so it never lands in the middle of a line.
#[cfg(feature = "hw-log")]
pub fn settle_screen() {
    x86_64::without_interrupts(|| {
        while LOCK
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        LOCK.owner.store(x86_64::cpu::index(), Ordering::Relaxed);
        // SAFETY: the console lock is held.
        unsafe { crate::screen_log::settle() };
        LOCK.owner.store(NO_OWNER, Ordering::Relaxed);
        LOCK.locked.store(false, Ordering::Release);
    });
}

/// Break the console lock, but only if this CPU is the one holding it.
///
/// For the panic and trap paths, where the faulting CPU may itself be the
/// holder and would otherwise wait for itself forever. The owner check is what
/// makes that safe with other CPUs running: a lock held by a different core is
/// left alone and simply waited for, because that core is still writing through
/// it and will release it shortly.
///
/// # Safety
/// Caller must be on a path that never returns to normal execution, so the
/// interrupted print is never resumed.
pub unsafe fn force_unlock() {
    if LOCK.owner.load(Ordering::Relaxed) == x86_64::cpu::index() {
        LOCK.owner.store(NO_OWNER, Ordering::Relaxed);
        LOCK.locked.store(false, Ordering::Release);
    }
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
