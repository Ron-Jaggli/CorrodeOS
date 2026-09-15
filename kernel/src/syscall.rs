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
//!   rdi  capability slot         -> sending task id
//!   rsi  message label           -> message label
//!   rdx  word 0                  -> word 0 / (identify) kind and rights
//!   r10  word 1                  -> word 1
//!   r8   word 2                  -> word 2
//!   r9   word 3                  -> word 3
//! ```
//!
//! `SYS_RECV` is the one that reads differently: `rsi` and `rdx` carry the
//! slots for an incoming capability and for the reply capability, since a
//! receive has no message to send.
//!
//! A send or a call can carry a capability, named by two more registers:
//!
//! ```text
//!   r12  capability to transfer, or NO_SLOT
//!   r13  mask to narrow it by
//! ```
//!
//! `r12` and `r13` rather than `rbx` or `rbp`: `rcx` and `r11` are out because
//! fast syscalls clobber them, and of what remains these are the two least
//! likely to be wanted by a userspace runtime for a frame or thread pointer.
//!
//! # Capabilities are usable, not just holdable
//!
//! Ring 3 gets `SYS_COPY` and `SYS_DELETE` as well, and can transfer a
//! capability in a message. Without those a user task could use what it was
//! handed and nothing more -- it could not narrow a capability before passing
//! it on, which is the operation the whole model is built around. A userspace
//! driver has to be able to hand a client authority over one thing without
//! handing over its own.
//!
//! Every rule still holds on this side of the boundary, because it is the same
//! code enforcing it: narrowing intersects, so asking for more than you hold
//! gets you less; and transferring requires `GRANT` on what is being sent.
//!
//! `r10` rather than `rcx` for the fourth argument. Nothing forces that today --
//! we arrive through an `int` gate, which preserves rcx -- but `syscall`
//! clobbers rcx with the return address, so using r10 now means moving to fast
//! syscalls later does not change the ABI underneath existing user code.
//!
//! # Receiving
//!
//! A server needs to nominate two more slots than a client: one for a
//! capability transferred with a message, one for the capability that answers a
//! call. Both are passed to `SYS_RECV` as slot indices, or [`NO_SLOT`].
//!
//! What a receive deliberately does *not* return is whether a reply capability
//! arrived. The receiver named the slot, so it can simply ask what is in it
//! with `SYS_IDENTIFY` -- which is the capability-shaped question anyway, and
//! costs no register in an ABI where the spare ones (`rcx`, `r11`) are exactly
//! the two that fast syscalls clobber.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::x86_64::idt::TrapFrame;
use crate::cap::{self, CapError, Rights};
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
/// Block until a message arrives on an endpoint capability. Needs READ.
pub const SYS_RECV: u64 = 4;
/// Answer a caller, spending the reply capability a receive produced.
pub const SYS_REPLY: u64 = 5;
/// Copy a capability within the caller's own space, narrowing by a mask.
pub const SYS_COPY: u64 = 6;
/// Drop a capability. The object goes away with its last one.
pub const SYS_DELETE: u64 = 7;

/// Slot argument meaning "nowhere".
///
/// A sentinel rather than a separate flags word: every real slot is a small
/// index, so the one value that can never be a slot is free to mean the absence
/// of one.
pub const NO_SLOT: u64 = u64::MAX;

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
            frame.rax = status(ipc::send(slot, message, transfer(frame)));
        }

        SYS_COPY => match cap::copy(
            slot,
            frame.rsi as usize,
            Rights::from_mask_bits(frame.rdx as u32),
        ) {
            Ok(granted) => {
                frame.rax = STATUS_OK;
                // What was actually granted, which may be less than asked for
                // and can never be more.
                frame.rdx = granted.bits() as u64;
            }
            Err(error) => frame.rax = error.code(),
        },

        SYS_DELETE => {
            frame.rax = status(cap::delete(slot));
        }

        SYS_RECV => {
            // Read both slot arguments before anything overwrites them: rdx is
            // an argument on the way in and a message word on the way out.
            let into = optional_slot(frame.rsi);
            let reply_into = optional_slot(frame.rdx);

            match ipc::recv(slot, into, reply_into) {
                Ok(received) => {
                    frame.rax = STATUS_OK;
                    frame.rdi = received.from as u64;
                    frame.rsi = received.message.label;
                    frame.rdx = received.message.words[0];
                    frame.r10 = received.message.words[1];
                    frame.r8 = received.message.words[2];
                    frame.r9 = received.message.words[3];
                }
                Err(error) => frame.rax = error.code(),
            }
        }

        SYS_REPLY => {
            frame.rax = status(ipc::reply(slot, message, None));
        }

        SYS_CALL => match ipc::call(slot, message, transfer(frame), None) {
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

/// Decode the capability-transfer arguments of a send or a call.
///
/// The mask is not checked here. `derive_for_transfer` intersects it against
/// what the caller holds and requires `GRANT`, so a caller asking for rights it
/// does not have receives less rather than being refused -- and a caller with
/// no `GRANT` is refused whatever it asks for.
fn transfer(frame: &TrapFrame) -> Option<(usize, Rights)> {
    let slot = optional_slot(frame.r12)?;
    Some((slot, Rights::from_mask_bits(frame.r13 as u32)))
}

/// Decode a slot argument, where [`NO_SLOT`] means none was given.
///
/// Any other value is passed through untouched, including one far past the end
/// of the capability space. Clamping or rejecting here would duplicate a check
/// the capability space already makes, and duplicated checks drift: `OutOfRange`
/// should be decided in exactly one place.
fn optional_slot(value: u64) -> Option<usize> {
    if value == NO_SLOT {
        None
    } else {
        Some(value as usize)
    }
}

fn status(result: Result<(), CapError>) -> u64 {
    match result {
        Ok(()) => STATUS_OK,
        Err(error) => error.code(),
    }
}
