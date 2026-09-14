//! The system call boundary.
//!
//! This is the entire interface a ring 3 task has to the kernel. Everything it
//! can do, it does by naming a capability slot in its own capability space --
//! there is no call here that takes an object, an address, or an identity.
//!
//! # Why there are no pointers
//!
//! A syscall that takes a user pointer has to validate it: in range, mapped,
//! user-accessible, and still all three by the time the kernel dereferences it.
//! That check is the classic place kernels get owned, and nothing here needs it.
//! An IPC [`Message`](crate::ipc::Message) is a label and four words -- it fits
//! in registers, so it crosses the boundary *by value*. The kernel never follows
//! a pointer the caller supplied, because the caller never supplies one.
//!
//! That is not a permanent restriction so much as a useful forcing function: it
//! keeps the boundary small while userspace is young. Bulk transfer, when it is
//! needed, should be a shared Frame the receiver already holds a capability for
//! -- authority it was granted, not an address it asserted.
//!
//! # Register convention
//!
//! ```text
//!   rax  syscall number          -> status (0 = ok, else CapError::code)
//!   rdi  capability slot         -> (call) sending task id
//!   rsi  message label           -> (call) reply label
//!   rdx  word 0                  -> (call) reply word 0 / (identify) kind+rights
//!   r10  word 1                  -> (call) reply word 1
//!   r8   word 2                  -> (call) reply word 2
//!   r9   word 3                  -> (call) reply word 3
//! ```
//!
//! `r10` rather than `rcx` for the fourth argument. Nothing forces that today --
//! we arrive through an `int` gate, which preserves rcx -- but `syscall`
//! clobbers rcx with the return address, so using r10 now means moving to fast
//! syscalls later does not change the ABI underneath existing user code.
//!
//! # Not yet
//!
//! No receive. A ring 3 task can be a client but not a server, because a server
//! also needs to nominate slots for transferred and reply capabilities and that
//! is a wider boundary than anything here is tested against. Kernel tasks serve;
//! user tasks call. Lifting that is the next increment.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::x86_64::idt::TrapFrame;
use crate::cap::{self, CapError};
use crate::ipc;
use crate::task;

/// Give up the rest of this time slice.
pub const SYS_YIELD: u64 = 0;
/// Report the kind and rights of a capability slot. Needs no rights: asking
/// what you hold is not an exercise of it.
pub const SYS_IDENTIFY: u64 = 1;
/// One-way send on an endpoint capability. Needs WRITE.
pub const SYS_SEND: u64 = 2;
/// Send and block for an answer. Needs WRITE, same as a send.
pub const SYS_CALL: u64 = 3;

/// Status returned for a syscall number we do not implement.
///
/// Deliberately not an error a capability operation can produce, so "you asked
/// for something that does not exist" never gets confused with "you asked for
/// something you are not allowed to do".
pub const STATUS_BAD_SYSCALL: u64 = 255;
pub const STATUS_OK: u64 = 0;

/// Every code selector seen entering this gate, OR-ed together.
///
/// The boot checks read it to confirm a user task really reached ring 3 rather
/// than merely being *called* a user task: the CPU writes CS into the trap
/// frame itself, so this cannot be faked by the code being measured.
static SEEN_CS: AtomicU64 = AtomicU64::new(0);

/// Bitwise OR of every CS that has entered a syscall.
pub fn seen_cs() -> u64 {
    SEEN_CS.load(Ordering::Relaxed)
}

/// Handle one system call.
///
/// Returns the frame to resume, which is not always the one passed in: a
/// syscall can block in IPC, and the task that comes back is then somebody
/// else. That is the same mechanism a timer tick uses, which is the point of
/// routing syscalls through the ordinary trap path.
pub fn dispatch(frame: &mut TrapFrame) -> *mut TrapFrame {
    SEEN_CS.fetch_or(frame.cs, Ordering::Relaxed);

    let slot = frame.rdi as usize;
    let message = ipc::Message {
        label: frame.rsi,
        words: [frame.rdx, frame.r10, frame.r8, frame.r9],
    };

    match frame.rax {
        SYS_YIELD => {
            frame.rax = STATUS_OK;
            return task::on_tick(frame as *mut TrapFrame);
        }

        SYS_IDENTIFY => match cap::identify(slot) {
            Ok((kind, rights)) => {
                frame.rax = STATUS_OK;
                frame.rdx = ((kind as u64) << 8) | rights.bits() as u64;
            }
            Err(error) => frame.rax = error.code(),
        },

        SYS_SEND => {
            frame.rax = status(ipc::send(slot, message, None));
        }

        SYS_CALL => match ipc::call(slot, message, None, None) {
            Ok(answer) => {
                frame.rax = STATUS_OK;
                frame.rdi = answer.from as u64;
                frame.rsi = answer.message.label;
                frame.rdx = answer.message.words[0];
                frame.r10 = answer.message.words[1];
                frame.r8 = answer.message.words[2];
                frame.r9 = answer.message.words[3];
            }
            Err(error) => frame.rax = error.code(),
        },

        _ => frame.rax = STATUS_BAD_SYSCALL,
    }

    frame as *mut TrapFrame
}

fn status(result: Result<(), CapError>) -> u64 {
    match result {
        Ok(()) => STATUS_OK,
        Err(error) => error.code(),
    }
}
