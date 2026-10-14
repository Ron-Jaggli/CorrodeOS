//! The ring 3 runtime: what a program has instead of a standard library.
//!
//! A Corrode program is a `no_std` binary that starts at `_start` holding
//! whatever capabilities its parent put in its capability space, and
//! nothing else. This crate is the small amount of code every such program
//! would otherwise write for itself:
//!
//! * [`sys`] -- the system call instruction, once, with the register
//!   convention in one place;
//! * the calls themselves as functions that take slots and return `Result`;
//! * [`device`] -- a device's configuration space, registers and interrupt,
//!   for a driver;
//! * [`log`] -- `println!`, for a program that was given somewhere to print;
//! * the entry point and the panic handler.
//!
//! There is no allocator. A driver's memory is its stack, its statics, and
//! whatever it maps through a capability.
//!
//! # No ambient anything
//!
//! Every function here takes a slot number. That is not ceremony: it is the
//! whole model showing through. There is no `open`, no global namespace, no
//! "the console". A program that was handed nothing can call everything in
//! this crate and every call will fail the same way.

#![no_std]

pub use corrode_abi as abi;
pub use corrode_pci as pci;

pub mod device;
pub mod log;
pub mod sys;

use abi::error;

/// Why a system call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No capability in that slot.
    Empty,
    OutOfRange,
    Occupied,
    /// Held, but without the rights the operation needs.
    InsufficientRights,
    /// Held, but pointing at the wrong kind of object.
    WrongType,
    NoSuchTask,
    Stale,
    Refused,
    NoSpace,
    /// The capability is good and the request is not.
    Invalid,
    /// The kernel does not have that system call.
    BadSyscall,
    /// A status this runtime was built before.
    Unknown(u64),
}

impl Error {
    /// The number the kernel reported, for passing on in a message.
    pub fn code(self) -> u64 {
        match self {
            Error::Empty => error::EMPTY,
            Error::OutOfRange => error::OUT_OF_RANGE,
            Error::Occupied => error::OCCUPIED,
            Error::InsufficientRights => error::INSUFFICIENT_RIGHTS,
            Error::WrongType => error::WRONG_TYPE,
            Error::NoSuchTask => error::NO_SUCH_TASK,
            Error::Stale => error::STALE,
            Error::Refused => error::REFUSED,
            Error::NoSpace => error::NO_SPACE,
            Error::Invalid => error::INVALID,
            Error::BadSyscall => abi::STATUS_BAD_SYSCALL,
            Error::Unknown(code) => code,
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// The status of a result as the number it crossed the boundary as: zero
/// for success. For a program reporting on itself in a message.
pub fn status<T>(result: &Result<T>) -> u64 {
    match result {
        Ok(_) => abi::STATUS_OK,
        Err(error) => error.code(),
    }
}

fn check(status: u64) -> Result<()> {
    Err(match status {
        abi::STATUS_OK => return Ok(()),
        error::EMPTY => Error::Empty,
        error::OUT_OF_RANGE => Error::OutOfRange,
        error::OCCUPIED => Error::Occupied,
        error::INSUFFICIENT_RIGHTS => Error::InsufficientRights,
        error::WRONG_TYPE => Error::WrongType,
        error::NO_SUCH_TASK => Error::NoSuchTask,
        error::STALE => Error::Stale,
        error::REFUSED => Error::Refused,
        error::NO_SPACE => Error::NoSpace,
        error::INVALID => Error::Invalid,
        abi::STATUS_BAD_SYSCALL => Error::BadSyscall,
        other => Error::Unknown(other),
    })
}

/// A message: a label and four words, by value. It is never a pointer into
/// anybody's memory, which is why it can cross the boundary in registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub label: u64,
    pub words: [u64; 4],
}

impl Message {
    pub const fn new(label: u64, words: [u64; 4]) -> Self {
        Self { label, words }
    }

    fn args(&self, slot: usize) -> sys::Args {
        sys::Args {
            rdi: slot as u64,
            rsi: self.label,
            rdx: self.words[0],
            r10: self.words[1],
            r8: self.words[2],
            r9: self.words[3],
            ..sys::Args::NONE
        }
    }
}

/// What a receive, or the answer to a call, produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// The sender's task id, or [`abi::FROM_HARDWARE`]. Information, not
    /// authority.
    pub from: u64,
    pub message: Message,
}

impl Received {
    fn from_registers(out: &sys::Out) -> Self {
        Received {
            from: out.rdi,
            message: Message {
                label: out.rsi,
                words: [out.rdx, out.r10, out.r8, out.r9],
            },
        }
    }
}

fn slot_or_none(slot: Option<usize>) -> u64 {
    slot.map_or(abi::NO_SLOT, |slot| slot as u64)
}

/// Give up the rest of this time slice.
pub fn yield_now() {
    sys::call(abi::SYS_YIELD, sys::Args::NONE);
}

/// Stop, for good.
pub fn exit() -> ! {
    loop {
        sys::call(abi::SYS_EXIT, sys::Args::NONE);
        // Only reached on a kernel with no such call. Still never return:
        // there is nothing under `_start` to return to.
        yield_now();
    }
}

/// What is in a slot: the kind of object and the rights held on it, as the
/// numbers in [`abi::kind`] and [`abi::rights`]. Needs no rights to ask.
pub fn identify(slot: usize) -> Result<(u8, u32)> {
    let out = sys::call(
        abi::SYS_IDENTIFY,
        sys::Args {
            rdi: slot as u64,
            ..sys::Args::NONE
        },
    );
    check(out.rax)?;
    Ok(((out.rdx >> 8) as u8, out.rdx as u32 & 0xff))
}

/// Send a message one way. Blocks until a receiver takes it.
pub fn send(endpoint: usize, message: Message) -> Result<()> {
    check(sys::call(abi::SYS_SEND, message.args(endpoint)).rax)
}

/// Send a message and a capability with it, narrowed by `mask`. Needs GRANT
/// on the capability being sent.
pub fn send_with(endpoint: usize, message: Message, capability: usize, mask: u32) -> Result<()> {
    let args = sys::Args {
        r12: capability as u64,
        r13: mask as u64,
        ..message.args(endpoint)
    };
    check(sys::call(abi::SYS_SEND, args).rax)
}

/// Send a message and block until it is answered.
///
/// `into` is where a capability arriving in the answer should land.
pub fn call(endpoint: usize, message: Message, into: Option<usize>) -> Result<Received> {
    let args = sys::Args {
        r14: slot_or_none(into),
        ..message.args(endpoint)
    };
    let out = sys::call(abi::SYS_CALL, args);
    check(out.rax)?;
    Ok(Received::from_registers(&out))
}

/// Block until a message arrives.
///
/// `into` is where a capability sent with it should land, and `reply` where
/// the capability to answer it should, if it turns out to be a call. Passing
/// no `reply` declares a server that takes one-way messages only.
pub fn recv(endpoint: usize, into: Option<usize>, reply: Option<usize>) -> Result<Received> {
    let out = sys::call(
        abi::SYS_RECV,
        sys::Args {
            rdi: endpoint as u64,
            rsi: slot_or_none(into),
            rdx: slot_or_none(reply),
            ..sys::Args::NONE
        },
    );
    check(out.rax)?;
    Ok(Received::from_registers(&out))
}

/// Answer a call, spending the reply capability the receive produced.
pub fn reply(reply: usize, message: Message) -> Result<()> {
    check(sys::call(abi::SYS_REPLY, message.args(reply)).rax)
}

/// Answer a call and hand over a capability with the answer.
pub fn reply_with(reply: usize, message: Message, capability: usize, mask: u32) -> Result<()> {
    let args = sys::Args {
        r12: capability as u64,
        r13: mask as u64,
        ..message.args(reply)
    };
    check(sys::call(abi::SYS_REPLY, args).rax)
}

/// Copy a capability into another of our own slots, narrowed by `mask`.
/// Returns the rights the copy actually has.
pub fn copy(from: usize, to: usize, mask: u32) -> Result<u32> {
    let out = sys::call(
        abi::SYS_COPY,
        sys::Args {
            rdi: from as u64,
            rsi: to as u64,
            rdx: mask as u64,
            ..sys::Args::NONE
        },
    );
    check(out.rax)?;
    Ok(out.rdx as u32)
}

/// Drop a capability.
pub fn delete(slot: usize) -> Result<()> {
    check(
        sys::call(
            abi::SYS_DELETE,
            sys::Args {
                rdi: slot as u64,
                ..sys::Args::NONE
            },
        )
        .rax,
    )
}

/// Memory the kernel mapped for us: where it is and how long.
///
/// The address was the kernel's choice. The length is in bytes and is as
/// trustworthy as the kernel, which is the only thing a program can say
/// about anything it did not compute itself.
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
    pub address: *mut u8,
    pub len: usize,
}

/// Map what a capability names -- device memory, for now -- into this
/// address space.
pub fn map(slot: usize) -> Result<Mapping> {
    let out = sys::call(
        abi::SYS_MAP,
        sys::Args {
            rdi: slot as u64,
            ..sys::Args::NONE
        },
    );
    check(out.rax)?;
    Ok(Mapping {
        address: out.rdx as *mut u8,
        len: out.r10 as usize,
    })
}

/// Name the function a program starts in.
///
/// ```ignore
/// corrode_rt::entry!(main);
/// fn main() { /* ... */ }
/// ```
///
/// The kernel enters `_start` on a fresh stack with nothing above it, so
/// returning from `main` exits.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        #[link_section = ".text._start"]
        pub extern "C" fn _start() -> ! {
            let main: fn() = $main;
            main();
            $crate::exit()
        }
    };
}

/// A panic says what it can, where it can, and stops.
///
/// If the program was given a log it goes there. If not there is nowhere to
/// say anything, and the program ends exactly as quietly as a task with an
/// empty capability space should.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("panic: {}", info);
    exit()
}
