//! Synchronous IPC over endpoints.
//!
//! A send and a receive meet at an endpoint. Whichever arrives first blocks
//! until the other shows up, and the message is copied directly between the two
//! tasks' control blocks. The kernel never buffers a message and never
//! allocates one, so there is no queue to size, no queue to exhaust, and no
//! way for one task to consume kernel memory by sending to a peer that is not
//! listening.
//!
//! # Authority
//!
//! Endpoints are objects like any other:
//!
//! * `WRITE` on the endpoint capability means *may send*.
//! * `READ` means *may receive*.
//!
//! Those are separable, which is the point. A client gets a send-only
//! capability and a server gets a receive-only one on the same endpoint, so the
//! client cannot impersonate the server by listening on it.
//!
//! Transferring a capability inside a message requires `GRANT` on the
//! capability being transferred -- the same rule as handing one over directly,
//! because that is exactly what it is.
//!
//! # Call and reply
//!
//! [`send`] is one-way: it tells the sender its message was taken and nothing
//! more. A service needs to answer, and the shape of that answer is the whole
//! security question -- a server must be able to reply to *the client that
//! asked* without being handed a general capability to talk to it, and without
//! the client having to hand out an endpoint that a malicious server could keep
//! and reuse later.
//!
//! [`call`] sends and then blocks for an answer. The kernel hands the receiver
//! a **reply capability** (see [`cap::reply_capability`]) naming that one parked
//! caller. [`reply`] spends it. It is one-shot in two independent ways:
//!
//! * it is consumed from the server's capability space on a successful reply;
//! * it carries the call's sequence number, so a copy kept behind goes stale
//!   the instant the caller is released, and a later call by the same task gets
//!   a different number.
//!
//! So a server cannot answer twice, cannot answer a call it was not given, and
//! cannot hoard the right to interrupt a client later. It is also issued
//! without `GRANT`: the right to answer your client is not yours to pass on.
//!
//! The asymmetry is deliberate. A client needs no new authority to be answered
//! -- it is already waiting -- so `call` requires nothing beyond the `WRITE` a
//! plain send needs.
//!
//! # Known gaps
//!
//! A caller blocked in `call` waits forever if the server never replies. That
//! is a real denial of service and the honest fix is a timeout or a fault
//! handler capability, neither of which exists yet; it is recorded rather than
//! papered over. Note what it is *not*: the caller chose that server by holding
//! a capability to its endpoint, so this is a service you trusted failing you,
//! not an attack any task can mount on any other.

use alloc::collections::VecDeque;

use crate::arch::x86_64;
use crate::cap::{self, CapError, CapPtr, Capability, Object, ObjectKind, Rights};
use crate::sync::SpinLock;
use crate::task::{self, State};

/// Inline message payload. Fixed size and `Copy`: a message is data, never a
/// pointer into the sender's memory.
#[derive(Clone, Copy, Debug)]
pub struct Message {
    pub label: u64,
    pub words: [u64; 4],
}

/// Bytes that fit in a message's inline words.
pub const TEXT_CAPACITY: usize = 32;

impl Message {
    pub const EMPTY: Message = Message {
        label: 0,
        words: [0; 4],
    };

    /// Pack short text into the inline words, truncating past 32 bytes.
    pub fn from_text(label: u64, text: &str) -> Message {
        let mut words = [0u64; 4];
        for (i, byte) in text.as_bytes().iter().take(TEXT_CAPACITY).enumerate() {
            words[i / 8] |= (*byte as u64) << ((i % 8) * 8);
        }
        Message { label, words }
    }

    /// A message carrying a single number, for replies that are just a result.
    pub const fn from_word(label: u64, value: u64) -> Message {
        Message {
            label,
            words: [value, 0, 0, 0],
        }
    }

    /// Unpack the inline words back into bytes. Returns the length up to the
    /// first NUL.
    pub fn text_bytes(&self, out: &mut [u8; TEXT_CAPACITY]) -> usize {
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = (self.words[i / 8] >> ((i % 8) * 8)) as u8;
        }
        out.iter().position(|b| *b == 0).unwrap_or(TEXT_CAPACITY)
    }
}

/// Names the one caller a reply capability may answer, and the one call.
///
/// Both halves matter. Without `task` the capability would answer anybody;
/// without `seq` it would answer that task's *next* call as happily as the one
/// it was issued for, which is how a server keeps a lever on a client long
/// after the exchange it was trusted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyToken {
    caller: usize,
    seq: u64,
}

impl ReplyToken {
    /// The task this token answers. Information, not authority.
    pub fn caller(&self) -> usize {
        self.caller
    }
}

/// A message parked in a task's control block while it is blocked.
pub struct PendingMessage {
    pub message: Message,
    /// Present only while in transit, and taken by whoever places it into the
    /// receiver's capability space.
    pub capability: Option<Capability>,
    /// What was transferred, for reporting after the capability itself has been
    /// handed over.
    pub info: Option<(ObjectKind, Rights)>,
    pub from: usize,
    /// Set when the sender is blocked in [`call`] awaiting an answer. `None` is
    /// a one-way send: nobody is waiting and there is nothing to reply to.
    pub reply: Option<ReplyToken>,
}

/// What a receive produced.
#[derive(Debug)]
pub struct Received {
    pub message: Message,
    /// Sender's task id. Information, not authority: knowing who sent a message
    /// grants the receiver nothing it did not already hold.
    pub from: usize,
    /// Kind and rights of a capability delivered with the message, if any.
    pub capability: Option<(ObjectKind, Rights)>,
    /// Slot now holding a one-shot capability to answer this message, present
    /// only when it arrived via [`call`].
    pub reply: Option<CapPtr>,
}

#[derive(Default)]
struct EndpointState {
    /// Tasks blocked in `send`/`call` with a message parked in `pending_out`.
    senders: VecDeque<usize>,
    /// Tasks blocked in `recv`.
    receivers: VecDeque<usize>,
}

pub struct Endpoint {
    state: SpinLock<EndpointState>,
}

impl Endpoint {
    pub fn new() -> Self {
        Self {
            state: SpinLock::new(EndpointState::default()),
        }
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve an endpoint capability with the rights an operation needs.
fn endpoint_for(slot: CapPtr, required: Rights) -> Result<alloc::sync::Arc<Object>, CapError> {
    let object = cap::lookup(slot, required)?;
    if object.kind() != ObjectKind::Endpoint {
        return Err(CapError::WrongType);
    }
    Ok(object)
}

/// Send a message. Blocks until a receiver takes it, then returns.
///
/// `transfer` names a capability in the caller's own space to send along, with
/// a mask to narrow it by. Sending a capability requires `GRANT` on it.
pub fn send(
    endpoint: CapPtr,
    message: Message,
    transfer: Option<(CapPtr, Rights)>,
) -> Result<(), CapError> {
    rendezvous(endpoint, message, transfer, None)?;
    Ok(())
}

/// Send a message and block until the receiver answers it.
///
/// `reply_into` is where a capability arriving *in the reply* should land.
///
/// Requires no more authority than [`send`]: being answered is not a new power,
/// it is the other half of the send the caller was already entitled to make.
pub fn call(
    endpoint: CapPtr,
    message: Message,
    transfer: Option<(CapPtr, Rights)>,
    reply_into: Option<CapPtr>,
) -> Result<Received, CapError> {
    let answer = rendezvous(endpoint, message, transfer, Some(reply_into))?;
    // `rendezvous` always produces an answer when given a reply slot; the
    // refusal and stale paths come back as errors above.
    answer.ok_or(CapError::Refused)
}

/// The shared body of [`send`] and [`call`].
///
/// `expect_reply` is `None` for a one-way send and `Some(slot)` for a call,
/// where `slot` is where a capability in the answer should be placed. Having
/// one function means the rendezvous, the rights check and the queue discipline
/// exist once rather than in two copies that drift.
fn rendezvous(
    endpoint: CapPtr,
    message: Message,
    transfer: Option<(CapPtr, Rights)>,
    expect_reply: Option<Option<CapPtr>>,
) -> Result<Option<Received>, CapError> {
    let me = task::current_id().ok_or(CapError::NoSuchTask)?;
    let object = endpoint_for(endpoint, Rights::WRITE)?;
    let Object::Endpoint(ep) = &*object else {
        return Err(CapError::WrongType);
    };

    // Derive the payload before anything is queued, so a rights failure cannot
    // leave a half-built message on an endpoint.
    let payload = match transfer {
        Some((slot, mask)) => Some(cap::derive_for_transfer(slot, mask)?),
        None => None,
    };
    let info = payload.as_ref().map(|c| (c.kind(), c.rights()));

    // Claim this call's sequence number up front. It is what makes the reply
    // capability we are about to hand out refer to *this* call and no other.
    let token = expect_reply.map(|_| ReplyToken {
        caller: me,
        seq: task::with_task(me, |task| {
            task.call_seq += 1;
            task.call_seq
        })
        .unwrap_or(0),
    });

    let outcome = x86_64::without_interrupts(|| {
        let mut state = ep.state.lock();

        if let Some(receiver) = state.receivers.pop_front() {
            let pending = PendingMessage {
                message,
                capability: payload.clone(),
                info,
                from: me,
                reply: token,
            };
            if !deliver(receiver, pending) {
                // The receiver has no free slot for the reply capability, so
                // this call could never be answered. Say so now rather than
                // parking the caller on a promise nobody can keep. The receiver
                // goes back where it was, still waiting and none the wiser --
                // nothing was taken from it, so there is nothing to explain.
                state.receivers.push_front(receiver);
                return Err(CapError::Refused);
            }
            // A one-way send is done the moment it is taken. A call is not: we
            // block here for the answer, which is why this is set before the
            // endpoint lock is dropped.
            if let Some(token) = token {
                block_for_reply(me, token, expect_reply.flatten());
            }
            Ok(token.is_some())
        } else {
            // Park the message on ourselves and block. Setting Blocked before
            // releasing the endpoint lock means a receiver cannot arrive, find
            // us queued, and wake us before we have actually parked anything.
            task::with_task(me, |task| {
                task.pending_out = Some(PendingMessage {
                    message,
                    capability: payload.clone(),
                    info,
                    from: me,
                    reply: token,
                });
                task.state = State::Blocked;
            });
            if let Some(token) = token {
                block_for_reply(me, token, expect_reply.flatten());
            }
            state.senders.push_back(me);
            Ok(false)
        }
    })?;

    if !outcome {
        // One-way: wait for a receiver to take it. `pending_out` going back to
        // None is the acknowledgement. Re-checking in a loop rather than
        // trusting a single wakeup means a spurious reschedule cannot lose the
        // event.
        //
        // A call falls through here too when it had to park: its message is
        // taken first and its answer arrives later, so it waits twice.
        loop {
            let taken = task::with_task(me, |task| task.pending_out.is_none()).unwrap_or(true);
            if taken {
                break;
            }
            task::yield_now();
        }
        if token.is_none() {
            return Ok(None);
        }
    }

    // A call: wait for the answer to land in `pending_in`.
    await_reply(me).map(Some)
}

/// Mark a task as parked awaiting an answer to `token`'s call.
///
/// Called with interrupts disabled, before the endpoint lock is released, so
/// the receiver cannot be scheduled and reply before we are ready to be woken.
fn block_for_reply(me: usize, token: ReplyToken, reply_into: Option<CapPtr>) {
    task::with_task(me, |task| {
        task.awaiting_reply = Some(token.seq);
        task.recv_slot = reply_into;
        task.state = State::Blocked;
    });
}

/// Spin-yield until this task's call is resolved, one way or the other.
///
/// The two outcomes are distinguished without any extra state. `reply` clears
/// `awaiting_reply` and installs a message in the same uninterruptible step, so
/// finding the flag cleared with nothing delivered can only mean the other
/// resolution: a receiver refused the call. Nobody is coming, and saying so
/// beats waiting forever for a promise that was already withdrawn.
fn await_reply(me: usize) -> Result<Received, CapError> {
    loop {
        let resolved = task::with_task(me, |task| match task.pending_in.take() {
            Some(pending) => Some(Ok(pending)),
            None if task.awaiting_reply.is_none() => Some(Err(CapError::Refused)),
            None => None,
        })
        .flatten();

        match resolved {
            Some(Ok(pending)) => {
                return Ok(Received {
                    message: pending.message,
                    from: pending.from,
                    capability: pending.info,
                    reply: None,
                })
            }
            Some(Err(error)) => return Err(error),
            None => task::yield_now(),
        }
    }
}

/// Hand a message to a task blocked in `recv`.
///
/// Called with the endpoint lock held. The receiver is not running, so the
/// sender is the one that places any transferred capability.
///
/// Returns false, having changed nothing, if the message is a call and the
/// receiver has nowhere to put the reply capability. Refusing whole is what
/// lets the caller back out instead of blocking on an answer that can never
/// come; a partial delivery here would strand it.
fn deliver(receiver: usize, pending: PendingMessage) -> bool {
    task::with_task(receiver, |task| {
        if let Some(token) = pending.reply {
            // Placed first, and refused whole if it will not fit: the caller is
            // about to block on this answer.
            let Some(slot) = task.reply_slot else {
                return false;
            };
            if task.cspace.put(slot, cap::reply_capability(token)).is_err() {
                return false;
            }
        }

        if let (Some(slot), Some(capability)) = (task.recv_slot, pending.capability) {
            // A full slot loses the capability rather than the message. The
            // receiver still sees `info`, so the loss is visible rather than
            // silent.
            let _ = task.cspace.put(slot, capability);
        }
        task.recv_slot = None;
        task.reply_slot = None;
        task.pending_in = Some(PendingMessage {
            message: pending.message,
            capability: None,
            info: pending.info,
            from: pending.from,
            reply: pending.reply,
        });
        task.state = State::Ready;
        true
    })
    .unwrap_or(false)
}

/// Release a parked caller whose call this receiver cannot answer.
///
/// Its message is dropped, deliberately: half-serving a request whose reply is
/// structurally impossible is worse than declining it. Clearing `awaiting_reply`
/// without delivering anything is what [`await_reply`] reads as a refusal, so
/// the caller gets an error rather than waiting on an answer nobody can give.
fn refuse(state: &mut EndpointState, caller: usize) {
    state.senders.pop_front();
    task::with_task(caller, |task| {
        task.pending_out = None;
        task.awaiting_reply = None;
        task.recv_slot = None;
        task.state = State::Ready;
    });
}

/// Receive a message. Blocks until a sender arrives.
///
/// `into` is where a capability transferred with the message should land, and
/// `reply_into` is where the reply capability should land if the message turns
/// out to be a [`call`].
///
/// Passing `None` for `reply_into` declares a server that does one-way work
/// only. It still receives sends normally, including ones queued behind a call;
/// a *call* that reaches it is refused, and the caller -- not this receiver --
/// is the one told, with [`CapError::Refused`]. A server is never woken by a
/// request it has already declared it does not handle.
pub fn recv(
    endpoint: CapPtr,
    into: Option<CapPtr>,
    reply_into: Option<CapPtr>,
) -> Result<Received, CapError> {
    let me = task::current_id().ok_or(CapError::NoSuchTask)?;
    let object = endpoint_for(endpoint, Rights::READ)?;
    let Object::Endpoint(ep) = &*object else {
        return Err(CapError::WrongType);
    };

    let immediate = x86_64::without_interrupts(|| {
        let mut state = ep.state.lock();

        // Walk the queue rather than looking only at its head. A call we cannot
        // answer is declined and dropped, and whoever was behind it is served
        // normally -- otherwise one client asking a question this server does
        // not take would wedge the endpoint for every sender behind it.
        loop {
            let Some(&sender) = state.senders.front() else {
                task::with_task(me, |task| {
                    task.recv_slot = into;
                    task.reply_slot = reply_into;
                    task.state = State::Blocked;
                });
                state.receivers.push_back(me);
                return None;
            };

            // If this is a call, the reply capability has to land somewhere
            // before we commit to taking the message -- the same refuse-whole
            // rule as `deliver`, seen from the other side.
            let is_call = task::with_task(sender, |task| {
                task.pending_out.as_ref().and_then(|p| p.reply)
            })
            .flatten();

            let placed = match is_call {
                Some(token) => {
                    let landed = match reply_into {
                        Some(slot) => task::with_task(me, |task| {
                            task.cspace.put(slot, cap::reply_capability(token))
                        })
                        .and_then(Result::ok)
                        .map(|()| slot),
                        None => None,
                    };

                    let Some(slot) = landed else {
                        refuse(&mut state, token.caller);
                        continue;
                    };
                    Some(slot)
                }
                None => None,
            };

            state.senders.pop_front();
            let pending = task::with_task(sender, |task| {
                let pending = task.pending_out.take();
                // A caller stays blocked: its message has been taken but its
                // answer has not been given. Only a one-way sender is done here.
                if is_call.is_none() {
                    task.state = State::Ready;
                }
                pending
            })
            .flatten();

            return pending.map(|pending| (pending, placed));
        }
    });

    let (pending, reply_slot) = match immediate {
        Some(taken) => taken,
        None => {
            // Blocked: the sender delivered into us, placing any reply
            // capability in the slot we nominated before parking. Which slot
            // that was is not worth recording -- it is `reply_into`, still in
            // hand, and the message says whether one was issued.
            loop {
                match task::with_task(me, |task| task.pending_in.take()).flatten() {
                    Some(pending) => {
                        let slot = pending.reply.and(reply_into);
                        break (pending, slot);
                    }
                    None => task::yield_now(),
                }
            }
        }
    };

    // We are the running task on the immediate path, so we place the
    // transferred capability ourselves.
    if let (Some(slot), Some(capability)) = (into, pending.capability) {
        let _ = task::with_task(me, |task| task.cspace.put(slot, capability));
    }

    Ok(Received {
        message: pending.message,
        from: pending.from,
        capability: pending.info,
        reply: reply_slot,
    })
}

/// Answer a call, using the one-shot capability the call handed over.
///
/// Consumes the capability on success: the debt is settled and the slot is free
/// for the next client. A copy kept elsewhere is already dead, because the
/// caller's `awaiting_reply` no longer matches the token's sequence number.
pub fn reply(
    reply_cap: CapPtr,
    message: Message,
    transfer: Option<(CapPtr, Rights)>,
) -> Result<(), CapError> {
    let me = task::current_id().ok_or(CapError::NoSuchTask)?;
    let token = cap::reply_token(reply_cap)?;

    // Derive before touching the caller, so a rights failure cannot half-wake
    // it -- the same ordering rule as the send path.
    let payload = match transfer {
        Some((slot, mask)) => Some(cap::derive_for_transfer(slot, mask)?),
        None => None,
    };
    let info = payload.as_ref().map(|c| (c.kind(), c.rights()));

    let delivered = x86_64::without_interrupts(|| {
        task::with_task(token.caller, |task| {
            // The whole one-shot argument lives in this comparison. The caller
            // is only answerable while it is parked on *this* call.
            if task.awaiting_reply != Some(token.seq) {
                return false;
            }
            task.awaiting_reply = None;

            if let (Some(slot), Some(capability)) = (task.recv_slot, payload) {
                let _ = task.cspace.put(slot, capability);
            }
            task.recv_slot = None;
            task.pending_in = Some(PendingMessage {
                message,
                capability: None,
                info,
                from: me,
                reply: None,
            });
            task.state = State::Ready;
            true
        })
        .unwrap_or(false)
    });

    if !delivered {
        return Err(CapError::Stale);
    }

    // One-shot in the space as well as in the token.
    cap::delete(reply_cap)
}
