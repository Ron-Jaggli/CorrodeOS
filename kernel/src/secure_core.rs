//! One physical core kept away from everything else.
//!
//! A secure enclave in the shape of Apple's: a processor that ordinary kernel
//! and user code never runs on, holding a key nothing else can read, reached
//! through a narrow mailbox and nothing else.
//!
//! # What "kept away" means today
//!
//! * **A whole core, not a thread.** [`crate::smp`] picks a physical core from
//!   the CPUID topology and hands every thread on it to this module. The first
//!   becomes the secure CPU; the rest are parked. A sibling running anything
//!   else would share this core's caches, TLBs and predictors, and could
//!   measure the secure CPU through all of them.
//! * **Never scheduled.** Neither CPU becomes a task, gets an idle task, or is
//!   counted in [`crate::smp::online`]. Their dense indices are handed out
//!   after every general CPU's, so the scheduler's round-robin over
//!   `0..online` cannot land a task here.
//! * **One interrupt, and only while idle.** No timer runs here, and the task
//!   priority refuses every vector but [`vectors::SECURE_MAILBOX`]. Interrupts
//!   are unmasked only for the `hlt` in the idle loop, so the one interrupt
//!   frame ever saved on this core is saved where there is nothing to hide.
//! * **A key that never touches RAM.** Made by RDRAND straight into `xmm15`
//!   at start-up and used there by AES-NI. See [`aes`] for why that register
//!   is safe to keep it in.
//!
//! # The mailbox
//!
//! One page, [`MAILBOX`]: a state word and a message. The kernel writes a
//! request, sets the state to `REQUEST`, and sends the mailbox IPI. The secure
//! core serves it in place, sets `DONE`, and goes back to sleep. One request
//! at a time; callers queue on a lock. The data that crosses is the caller's
//! anyway -- plaintext in, ciphertext out -- and the key never crosses at all.
//!
//! # Sealing
//!
//! What ring 3 may ask for, through a [`crate::seal`] capability, is sealing:
//! AES-CCM ([`corrode_seal`]) under a key of the holder's own. Each holder is
//! a number, and its key is the encryption of a label naming it under the key
//! in `xmm15`, made afresh in a register for every block
//! ([`aes::encrypt_block_as`]). So one holder cannot open what another sealed,
//! and still no key is ever in memory. The secure core picks every nonce, from
//! a counter, so no caller can make it use one twice. Raw counter mode
//! ([`crypt`]) stays the kernel's, for its boot checks.
//!
//! # What it does not mean yet
//!
//! The kernel can still read this core's memory: its stack and the mailbox
//! are ordinary kernel memory, and ring 0 on any core could also run code here
//! to read `xmm15`. Taking that away needs a layer underneath the kernel
//! (second-level page tables) and the IOMMU, so that neither the other cores
//! nor a device can reach this core's frames. The last-level cache is still
//! shared with every other core, too, until it is partitioned.

mod aes;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::arch::x86_64::{self, cpu, lapic, vectors};
use crate::sync::SpinLock;

const NONE: usize = usize::MAX;

/// Dense index of the secure CPU, once it is running.
static SECURE: AtomicUsize = AtomicUsize::new(NONE);

/// Its LAPIC ID, for aiming the mailbox IPI.
static SECURE_APIC: AtomicU32 = AtomicU32::new(0);

/// Siblings that have reached [`park`].
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// Set by the secure CPU on its way into its loop, after its last write to
/// anything shared.
static READY: AtomicBool = AtomicBool::new(false);

/// What the secure CPU found it could do, as `HEALTH_*` bits.
static HEALTH: AtomicU32 = AtomicU32::new(0);
const HEALTH_AES: u32 = 1 << 0;
const HEALTH_SELF_TEST: u32 = 1 << 1;
const HEALTH_KEY: u32 = 1 << 2;

/// Set once a request has gone unanswered, or the secure CPU has taken a
/// fatal trap. Nothing is sent after that: a secure core that stopped
/// answering mid-request may still be writing.
static SILENT: AtomicBool = AtomicBool::new(false);

/// Set by the secure CPU once SSE is on there, which is what makes it legal
/// for [`on_fatal`] to touch `xmm15`.
static SSE_ON: AtomicBool = AtomicBool::new(false);

/// What came up.
pub struct Status {
    /// Dense index of the secure CPU, if one is running.
    pub secure: Option<usize>,
    /// How many of its siblings are parked.
    pub parked: usize,
    /// The CPU has AES-NI, RDRAND and SSE4.1.
    pub aes: bool,
    /// AES gave the FIPS-197 answer on this CPU.
    pub self_test: bool,
    /// A key was made and is held in a register.
    pub key: bool,
}

pub fn status() -> Status {
    let secure = SECURE.load(Ordering::Acquire);
    let health = HEALTH.load(Ordering::Acquire);
    Status {
        secure: (READY.load(Ordering::Acquire) && secure != NONE).then_some(secure),
        parked: PARKED.load(Ordering::Acquire),
        aes: health & HEALTH_AES != 0,
        self_test: health & HEALTH_SELF_TEST != 0,
        key: health & HEALTH_KEY != 0,
    }
}

// ---------------------------------------------------------------------------
// The mailbox
// ---------------------------------------------------------------------------

/// Most bytes one request can carry.
pub const PAYLOAD: usize = 4032;

const IDLE: u32 = 0;
const REQUEST: u32 = 1;
const DONE: u32 = 2;

const OP_PING: u32 = 1;
const OP_CRYPT: u32 = 2;
const OP_SEAL: u32 = 3;
const OP_UNSEAL: u32 = 4;

const STATUS_OK: u32 = 0;
const STATUS_UNKNOWN_OP: u32 = 1;
const STATUS_TOO_LONG: u32 = 2;
const STATUS_NO_KEY: u32 = 3;
/// Holder 0 is no one's: refused rather than given a key.
const STATUS_NO_HOLDER: u32 = 4;
/// The tag did not match: changed, cut short, or sealed by someone else.
pub const STATUS_FORGED: u32 = 5;

#[repr(C)]
struct Message {
    op: u32,
    status: u32,
    len: u32,
    /// Whose key, for sealing.
    holder: u32,
    nonce: [u8; 12],
    tag: [u8; corrode_seal::TAG],
    payload: [u8; PAYLOAD],
}

#[repr(C, align(4096))]
struct Mailbox {
    state: AtomicU32,
    message: UnsafeCell<Message>,
}

// SAFETY: `message` is written only by whoever `state` says owns it: the
// caller holding `CALLER` while it is IDLE or DONE, the secure core while it
// is REQUEST. Each handover is a release store matched by an acquire load.
unsafe impl Sync for Mailbox {}

static MAILBOX: Mailbox = Mailbox {
    state: AtomicU32::new(IDLE),
    message: UnsafeCell::new(Message {
        op: 0,
        status: 0,
        len: 0,
        holder: 0,
        nonce: [0; 12],
        tag: [0; corrode_seal::TAG],
        payload: [0; PAYLOAD],
    }),
};

/// One caller at a time.
static CALLER: SpinLock<()> = SpinLock::new(());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallError {
    /// No secure core came up, or it stopped answering earlier.
    NotRunning,
    /// More than [`PAYLOAD`] bytes.
    TooLong,
    /// The secure core did not answer in time.
    NoAnswer,
    /// The secure core answered and said no.
    Refused(u32),
}

/// Everything in a request but its payload, and in the answer.
struct Exchange {
    op: u32,
    holder: u32,
    nonce: [u8; 12],
    tag: [u8; corrode_seal::TAG],
}

impl Exchange {
    fn new(op: u32) -> Self {
        Self {
            op,
            holder: 0,
            nonce: [0; 12],
            tag: [0; corrode_seal::TAG],
        }
    }
}

/// Ask the secure core whether it is there.
pub fn ping() -> Result<(), CallError> {
    let mut reply = [0u8; 4];
    let len = call(&mut Exchange::new(OP_PING), &mut reply, 0)?;
    if len == 4 && &reply == b"pong" {
        Ok(())
    } else {
        Err(CallError::Refused(STATUS_UNKNOWN_OP))
    }
}

/// Encrypt or decrypt `data` in place with the secure core's key: AES-128 in
/// counter mode, the block counter starting at 1 after a 96-bit `nonce`.
/// The same call undoes itself.
///
/// Counter mode alone is malleable -- nothing detects a flipped bit -- so this
/// is a building block for sealing, not sealing. A nonce must never be reused
/// with different data.
pub fn crypt(nonce: &[u8; 12], data: &mut [u8]) -> Result<(), CallError> {
    let len = data.len();
    let mut exchange = Exchange::new(OP_CRYPT);
    exchange.nonce = *nonce;
    call(&mut exchange, data, len).map(|_| ())
}

/// Seal `data` in place under `holder`'s key. Returns the nonce the secure
/// core chose and the tag; [`unseal`] needs both.
pub fn seal(holder: u32, data: &mut [u8]) -> Result<([u8; 12], [u8; corrode_seal::TAG]), CallError> {
    let len = data.len();
    let mut exchange = Exchange::new(OP_SEAL);
    exchange.holder = holder;
    call(&mut exchange, data, len)?;
    Ok((exchange.nonce, exchange.tag))
}

/// Open what [`seal`] made for the same `holder`, in place. Refused with
/// [`STATUS_FORGED`], and `data` zeroed, if anything about it has changed or
/// another holder sealed it.
pub fn unseal(
    holder: u32,
    nonce: &[u8; 12],
    tag: &[u8; corrode_seal::TAG],
    data: &mut [u8],
) -> Result<(), CallError> {
    let len = data.len();
    let mut exchange = Exchange::new(OP_UNSEAL);
    exchange.holder = holder;
    exchange.nonce = *nonce;
    exchange.tag = *tag;
    let result = call(&mut exchange, data, len).map(|_| ());
    if result.is_err() {
        data.fill(0);
    }
    result
}

/// Send one request and wait for the answer. `data` goes in as the payload
/// (its first `send` bytes) and comes back out, up to its length, and the
/// answer's nonce and tag come back in `exchange`. Returns the payload length
/// the secure core answered with.
fn call(exchange: &mut Exchange, data: &mut [u8], send: usize) -> Result<usize, CallError> {
    if data.len() > PAYLOAD {
        return Err(CallError::TooLong);
    }
    if status().secure.is_none() || SILENT.load(Ordering::Acquire) {
        return Err(CallError::NotRunning);
    }
    let apic = SECURE_APIC.load(Ordering::Relaxed) as u8;

    // Interrupts off while the lock is held, as for every lock: the wait is
    // short, and a caller preempted holding it would stall every other one.
    x86_64::without_interrupts(|| {
        let _guard = CALLER.lock();

        // Asked again now the lock is held: the caller ahead of this one may
        // be the one that gave up, and it left the mailbox with the secure
        // core. Only IDLE makes the message this caller's to write.
        if SILENT.load(Ordering::Acquire) || MAILBOX.state.load(Ordering::Acquire) != IDLE {
            return Err(CallError::NotRunning);
        }

        // SAFETY: the state is IDLE and `CALLER` is held, so the message is
        // this caller's alone until the release store below.
        unsafe {
            let message = &mut *MAILBOX.message.get();
            message.op = exchange.op;
            message.status = 0;
            message.len = send as u32;
            message.holder = exchange.holder;
            message.nonce = exchange.nonce;
            message.tag = exchange.tag;
            message.payload[..send].copy_from_slice(&data[..send]);
        }
        MAILBOX.state.store(REQUEST, Ordering::Release);
        lapic::send(apic, vectors::SECURE_MAILBOX);

        const ATTEMPTS: u64 = 500_000_000;
        let mut spins = 0u64;
        while MAILBOX.state.load(Ordering::Acquire) != DONE {
            spins += 1;
            // Out of patience, or the secure CPU said on its way down that no
            // answer is coming.
            if spins == ATTEMPTS || SILENT.load(Ordering::Acquire) {
                SILENT.store(true, Ordering::Release);
                return Err(CallError::NoAnswer);
            }
            core::hint::spin_loop();
        }

        // SAFETY: DONE, acquired above, hands the message back to the caller.
        let result = unsafe {
            let message = &mut *MAILBOX.message.get();
            let answered = message.len as usize;
            let result = if message.status != STATUS_OK {
                Err(CallError::Refused(message.status))
            } else if answered > PAYLOAD {
                Err(CallError::Refused(STATUS_TOO_LONG))
            } else {
                let len = answered.min(data.len());
                data[..len].copy_from_slice(&message.payload[..len]);
                exchange.nonce = message.nonce;
                exchange.tag = message.tag;
                Ok(answered)
            };
            // The mailbox is shared kernel memory, and what crossed it was one
            // caller's data. Leave none of it for the next.
            let used = send.max(answered).min(PAYLOAD);
            message.payload[..used].fill(0);
            message.nonce = [0; 12];
            message.tag = [0; corrode_seal::TAG];
            message.holder = 0;
            message.len = 0;
            result
        };
        MAILBOX.state.store(IDLE, Ordering::Release);
        result
    })
}

// ---------------------------------------------------------------------------
// The secure CPU
// ---------------------------------------------------------------------------

/// Where the secure CPU goes once it has the kernel's GDT, IDT and page tables.
///
/// Sets up its own state -- interrupt priority, SSE, the self-test, the key --
/// then announces itself and serves the mailbox forever.
pub fn run(cpu: usize) -> ! {
    // Who this is, before anything that could fault: [`on_fatal`] has to
    // recognise this CPU from here on. `status` and `call` wait for READY.
    SECURE_APIC.store(lapic::id(), Ordering::Relaxed);
    SECURE.store(cpu, Ordering::Release);

    lapic::enable_local();
    lapic::accept_only_from(vectors::SECURE_MAILBOX);

    let mut health = 0;
    if aes::supported() {
        health |= HEALTH_AES;
        // SAFETY: ring 0, and nothing on this core relies on SSE being off.
        unsafe { x86_64::enable_sse() };
        SSE_ON.store(true, Ordering::Release);
        // SAFETY: SSE is on and the CPU has AES-NI. The self-test leaves the
        // test key in xmm15; only a passing one is followed by a real key,
        // and only a real key lets a request through.
        unsafe {
            if aes::self_test() && seal_self_test() {
                health |= HEALTH_SELF_TEST;
                if aes::generate_key() {
                    health |= HEALTH_KEY;
                }
            }
        }
    }
    HEALTH.store(health, Ordering::Release);
    READY.store(true, Ordering::Release);

    loop {
        if MAILBOX.state.load(Ordering::Acquire) == REQUEST {
            serve(health);
            MAILBOX.state.store(DONE, Ordering::Release);
            continue;
        }
        idle();
    }
}

/// Answer the request in the mailbox.
fn serve(health: u32) {
    // SAFETY: REQUEST, acquired by the caller of this function, hands the
    // message to this CPU until DONE is stored.
    let message = unsafe { &mut *MAILBOX.message.get() };
    // The request's words are read once each. The page is ordinary kernel
    // memory, and a length checked in one read and used from another is a
    // check that a stray write from any other CPU could step between.
    let op = message.op;
    let len = message.len as usize;
    let holder = message.holder;
    let nonce = message.nonce;
    let tag = message.tag;
    message.status = match op {
        OP_PING => {
            message.payload[..4].copy_from_slice(b"pong");
            message.len = 4;
            STATUS_OK
        }
        OP_CRYPT if health & HEALTH_KEY == 0 => STATUS_NO_KEY,
        OP_CRYPT if len > PAYLOAD => STATUS_TOO_LONG,
        OP_CRYPT => {
            counter_mode(&nonce, &mut message.payload[..len]);
            message.len = len as u32;
            STATUS_OK
        }
        OP_SEAL | OP_UNSEAL if health & HEALTH_KEY == 0 => STATUS_NO_KEY,
        OP_SEAL | OP_UNSEAL if len > PAYLOAD => STATUS_TOO_LONG,
        OP_SEAL | OP_UNSEAL if holder == 0 => STATUS_NO_HOLDER,
        OP_SEAL => {
            let nonce = next_nonce();
            message.tag = seal_as(holder, &nonce, &mut message.payload[..len]);
            message.nonce = nonce;
            message.len = len as u32;
            STATUS_OK
        }
        OP_UNSEAL => {
            message.len = len as u32;
            if unseal_as(holder, &nonce, &tag, &mut message.payload[..len]) {
                STATUS_OK
            } else {
                STATUS_FORGED
            }
        }
        _ => STATUS_UNKNOWN_OP,
    };
}

/// XOR `data` with the keystream for `nonce`. The keystream passes through
/// this stack a block at a time; the key it comes from does not.
fn counter_mode(nonce: &[u8; 12], data: &mut [u8]) {
    for (counter, chunk) in (1u32..).zip(data.chunks_mut(16)) {
        let mut block = [0u8; 16];
        block[..12].copy_from_slice(nonce);
        block[12..].copy_from_slice(&counter.to_be_bytes());
        // SAFETY: only reached with HEALTH_KEY set, which `run` sets only
        // after enabling SSE, checking for AES-NI and putting a key in xmm15.
        unsafe { aes::encrypt_block(&mut block) };
        for (byte, key) in chunk.iter_mut().zip(block) {
            *byte ^= key;
        }
    }
}

/// Seals made since boot; the next nonce. Touched only by the secure CPU.
static SEALS: AtomicU64 = AtomicU64::new(0);

/// A nonce no seal has used under this boot's key: four zero bytes and a
/// count. The key is new every boot, so the count may start again.
fn next_nonce() -> [u8; 12] {
    let count = SEALS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&count.to_be_bytes());
    nonce
}

/// What `holder`'s key is the encryption of.
fn label(holder: u32) -> [u8; 16] {
    let mut label = [0u8; 16];
    label[..12].copy_from_slice(b"corrode-seal");
    label[12..].copy_from_slice(&holder.to_be_bytes());
    label
}

/// Seal under `holder`'s key; see [`corrode_seal::seal`].
fn seal_as(holder: u32, nonce: &[u8; 12], data: &mut [u8]) -> [u8; corrode_seal::TAG] {
    let label = label(holder);
    // SAFETY: only reached with SSE on and AES-NI present, on this CPU: from
    // `serve` with HEALTH_KEY set, or from the start-up self-test.
    corrode_seal::seal(&mut |block| unsafe { aes::encrypt_block_as(&label, block) }, nonce, data)
}

/// Open under `holder`'s key; see [`corrode_seal::open`].
fn unseal_as(holder: u32, nonce: &[u8; 12], tag: &[u8; corrode_seal::TAG], data: &mut [u8]) -> bool {
    let label = label(holder);
    // SAFETY: as in `seal_as`.
    corrode_seal::open(&mut |block| unsafe { aes::encrypt_block_as(&label, block) }, nonce, data, tag)
}

/// Sealing on a known key, run while `xmm15` still holds FIPS-197's test key.
/// The answers came from a separate implementation (Python's `cryptography`,
/// AES-CCM under the derived key); they check the derivation and CCM together,
/// as the secure core runs them.
fn seal_self_test() -> bool {
    const PLAIN: &[u8; 25] = b"sealed by the secure core";
    const NONCE: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
    const SETTINGS: ([u8; 25], [u8; 16]) = (
        [
            0x99, 0xa5, 0x94, 0x38, 0xda, 0xa8, 0x28, 0xc1, 0x32, 0x32, 0xeb, 0xb2, 0x71, 0x03,
            0x5e, 0x04, 0x28, 0x91, 0x81, 0x0b, 0x39, 0x58, 0xc5, 0x50, 0x7d,
        ],
        [
            0x59, 0xff, 0x80, 0x5d, 0xb7, 0x9a, 0xd8, 0xfe, 0x28, 0x68, 0x9f, 0x54, 0xed, 0xee,
            0x62, 0xaa,
        ],
    );
    const LOCK_TAG: [u8; 16] = [
        0xbc, 0x70, 0x56, 0xbf, 0xdb, 0x53, 0x4b, 0xe6, 0x89, 0xb3, 0x6c, 0xd4, 0x01, 0xfc, 0x7c,
        0x05,
    ];
    let mut data = *PLAIN;
    let tag = seal_as(1, &NONCE, &mut data);
    let sealed = data == SETTINGS.0 && tag == SETTINGS.1;
    let mut other = *PLAIN;
    let other_tag = seal_as(2, &NONCE, &mut other);
    let opened = unseal_as(1, &NONCE, &tag, &mut data) && data == *PLAIN;
    let mut crossed = SETTINGS.0;
    let refused = !unseal_as(2, &NONCE, &tag, &mut crossed) && crossed == [0; 25];
    sealed && other_tag == LOCK_TAG && opened && refused
}

/// Sleep until the mailbox IPI. `sti` holds off interrupts for one more
/// instruction, so one arriving between the state check and here wakes the
/// `hlt` rather than being taken before it and missed.
fn idle() {
    // SAFETY: the IDT handles the mailbox vector, and the task priority keeps
    // every other one pending. Not `nomem`: the mailbox must be re-read after.
    unsafe {
        core::arch::asm!("sti", "hlt", "cli", options(nostack));
    }
}

/// Called first thing on the panic and unhandled-trap paths, on whichever CPU
/// took them. Does nothing unless that CPU is the secure one.
///
/// Those paths go on to print, and under `efi-log` to call the firmware --
/// which the UEFI calling convention entitles to save `xmm15` on its stack,
/// in RAM. So the key is destroyed before any of that runs, and callers are
/// told no answer is coming rather than left to time out.
pub fn on_fatal() {
    if !lapic::is_mapped() || SECURE.load(Ordering::Acquire) != cpu::index() {
        return;
    }
    SILENT.store(true, Ordering::Release);
    HEALTH.fetch_and(!HEALTH_KEY, Ordering::AcqRel);
    if SSE_ON.load(Ordering::Acquire) {
        // SAFETY: this is the secure CPU and it has turned SSE on.
        unsafe { aes::forget_key() };
    }
}

/// Where every other thread on the secure core goes.
///
/// Its local APIC is software-disabled, so fixed IPIs -- including the
/// broadcast HALT -- are not even accepted. A halted thread with interrupts
/// masked issues nothing, which leaves the core's shared resources to the
/// secure CPU alone. It has the kernel's IDT all the same, because an NMI can
/// still arrive and must find a handler rather than triple-fault the machine.
pub fn park(_cpu: usize) -> ! {
    // Said outright rather than assumed of whatever the bootloader left.
    lapic::disable_local();
    PARKED.fetch_add(1, Ordering::Release);
    loop {
        // SAFETY: masking interrupts and halting touch no memory. `hlt` can
        // still return -- an NMI or SMI wakes it -- hence the loop, and `cli`
        // again in case a handler returned with IF set.
        unsafe {
            core::arch::asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}

/// Spin until the secure CPU (if one was started) and `parked` siblings have
/// arrived, or give up. Bounded for the same reason as the general CPUs' wait:
/// a core that never arrives is reported, not waited on forever.
pub fn wait_for(secure: bool, parked: usize) {
    const ATTEMPTS: u64 = 200_000_000;
    let mut spins = 0u64;
    while !((!secure || READY.load(Ordering::Acquire)) && PARKED.load(Ordering::Acquire) >= parked)
        && spins < ATTEMPTS
    {
        core::hint::spin_loop();
        spins += 1;
    }
}
