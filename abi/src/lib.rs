//! The numbers the kernel and a ring 3 program have to agree on.
//!
//! While user programs were assembly inside the kernel image there was one
//! copy of each of these, because there was one crate. Now a program is a
//! separate binary built on its own, and a system call number that means one
//! thing to the kernel and another to the program is a bug no compiler will
//! find. So they live here, the kernel and the runtime both depend on this,
//! and neither says them twice.
//!
//! Nothing in this crate does anything. It is constants, and the layouts of
//! the small protocols spoken at boot.
//!
//! # Register convention
//!
//! ```text
//!   rax  syscall number          -> status (0 = ok, else an error code)
//!   rdi  capability slot         -> sending task id
//!   rsi  message label           -> message label
//!   rdx  word 0                  -> word 0 / (identify) kind and rights
//!   r10  word 1                  -> word 1
//!   r8   word 2                  -> word 2
//!   r9   word 3                  -> word 3
//!   r12  capability to transfer, or NO_SLOT
//!   r13  mask to narrow it by
//! ```
//!
//! Entry is `int 0x80`. See `kernel/src/syscall.rs` for why each register is
//! the one it is.

#![no_std]

/// The interrupt vector that is the system call gate.
pub const SYSCALL_VECTOR: u8 = 0x80;

/// Give up the rest of this time slice.
pub const SYS_YIELD: u64 = 0;
/// Report the kind and rights of a capability slot. Needs no rights: asking
/// what you hold is not an exercise of it.
pub const SYS_IDENTIFY: u64 = 1;
/// One-way send on an endpoint capability. Needs WRITE.
pub const SYS_SEND: u64 = 2;
/// Send and block for an answer. Needs WRITE, same as a send. `r14` is the
/// slot a capability arriving in the answer should land in, or `NO_SLOT`.
pub const SYS_CALL: u64 = 3;
/// Block until a message arrives on an endpoint capability. Needs READ.
///
/// On an interrupt capability, block until the device signals: the message
/// then comes from [`FROM_HARDWARE`] with the count in word 0.
pub const SYS_RECV: u64 = 4;
/// Answer a caller, spending the reply capability a receive produced. May
/// carry a capability, by `r12` and `r13` as a send does.
pub const SYS_REPLY: u64 = 5;
/// Copy a capability within the caller's own space, narrowing by a mask.
pub const SYS_COPY: u64 = 6;
/// Drop a capability. The object goes away with its last one.
pub const SYS_DELETE: u64 = 7;
/// Map what a capability names into the caller's address space. Needs READ,
/// and WRITE as well for the mapping to be writable. The kernel chooses the
/// address and returns it in `rdx`.
///
/// For a surface: the width and height in pixels in `r10` and `r8`, and the
/// bytes from one row to the next in `r9`. For device memory, ordinary
/// memory or a blob: the length in bytes in `r10`, and zero in the other two.
pub const SYS_MAP: u64 = 8;
/// Say that a rectangle of a surface has been redrawn. Needs WRITE. `rsi`
/// and `rdx` are its left and top, `r10` and `r8` its width and height.
pub const SYS_COMMIT: u64 = 9;
/// Read a register of a PCI device's configuration space. Needs READ. `rsi`
/// is the offset and `rdx` the width in bytes: 1, 2 or 4. The value comes
/// back in `rdx`.
pub const SYS_CONFIG_READ: u64 = 10;
/// Write one. Needs WRITE, and the register has to be one a driver may
/// write. `rsi` is the offset, `rdx` the width, `r10` the value.
pub const SYS_CONFIG_WRITE: u64 = 11;
/// Stop, for good. Takes nothing and does not return.
pub const SYS_EXIT: u64 = 12;
/// Where one page of a memory capability is in physical memory, for telling
/// a device. Needs WRITE. `rsi` is the page; the address comes back in `rdx`.
pub const SYS_PHYSICAL: u64 = 13;

/// Slot argument meaning "nowhere".
///
/// A sentinel rather than a separate flags word: every real slot is a small
/// index, so the one value that can never be a slot is free to mean the absence
/// of one.
pub const NO_SLOT: u64 = u64::MAX;

/// The sender of a message that no task sent: a device's interrupt, received
/// on an interrupt capability.
pub const FROM_HARDWARE: u64 = u64::MAX;

pub const STATUS_OK: u64 = 0;
/// Status returned for a syscall number the kernel does not implement.
///
/// Deliberately not an error a capability operation can produce, so "you asked
/// for something that does not exist" never gets confused with "you asked for
/// something you are not allowed to do".
pub const STATUS_BAD_SYSCALL: u64 = 255;

/// Why a capability operation failed, as it appears in `rax`.
///
/// Written out rather than derived from an enum's discriminant: a program
/// compiled against these numbers must not be broken by someone reordering
/// variants, and zero is reserved for success.
pub mod error {
    /// No capability in that slot.
    pub const EMPTY: u64 = 1;
    /// Slot index past the end of the capability space.
    pub const OUT_OF_RANGE: u64 = 2;
    /// Slot already holds a capability.
    pub const OCCUPIED: u64 = 3;
    /// Held, but without the rights this operation needs.
    pub const INSUFFICIENT_RIGHTS: u64 = 4;
    /// Held, but pointing at the wrong kind of object.
    pub const WRONG_TYPE: u64 = 5;
    pub const NO_SUCH_TASK: u64 = 6;
    /// A reply capability whose call is over.
    pub const STALE: u64 = 7;
    /// A call the receiver could not be made to answer.
    pub const REFUSED: u64 = 8;
    /// Nowhere to put a mapping.
    pub const NO_SPACE: u64 = 9;
    /// The capability is good and the request is not.
    pub const INVALID: u64 = 10;
}

/// What a capability points at, as `SYS_IDENTIFY` reports it in bits 8 and
/// up of `rdx`.
pub mod kind {
    pub const CONSOLE: u8 = 1;
    pub const FRAME: u8 = 2;
    pub const TASK: u8 = 3;
    pub const ENDPOINT: u8 = 4;
    pub const REPLY: u8 = 5;
    pub const SURFACE: u8 = 6;
    pub const PCI_DEVICE: u8 = 7;
    pub const DEVICE_MEMORY: u8 = 8;
    pub const INTERRUPT: u8 = 9;
    pub const MEMORY: u8 = 10;
    pub const BLOB: u8 = 11;
}

/// Rights, as bits: in the low byte of what `SYS_IDENTIFY` reports, and as
/// the mask a copy or a transfer narrows by.
pub mod rights {
    pub const READ: u32 = 1 << 0;
    pub const WRITE: u32 = 1 << 1;
    /// May hand the capability to another task.
    pub const GRANT: u32 = 1 << 2;
    pub const ALL: u32 = 0b111;
}

/// Where a program is put, and what it may assume about its address space.
pub mod layout {
    /// The lowest address a program's image may occupy. Well clear of the
    /// null page, so a jump through a zeroed pointer still faults.
    pub const IMAGE_BASE: u64 = 0x0000_0000_0040_0000;
    /// One past the highest: the guard page under the stack.
    pub const IMAGE_END: u64 = 0x0000_0000_1000_0000;
}

/// Saying things: the protocol of the log endpoint.
///
/// A message holds thirty-two bytes and the boundary takes no pointers, so a
/// line longer than that is sent in pieces. Every piece but the last is
/// `TEXT`; the last is `LINE`, and that is what puts it on the console. The
/// bytes are the message's four words, little-endian, and stop at the first
/// zero.
pub mod log {
    pub const TEXT: u64 = 1;
    pub const LINE: u64 = 2;
    /// Bytes in one message.
    pub const CAPACITY: usize = 32;
}

/// The boot-time conversation between the kernel and the `edu` driver, which
/// exists to prove a driver can be an ordinary program.
///
/// The slots are where the kernel puts the driver's capabilities before it
/// runs; the labels are what it reports back, in order.
pub mod edu {
    /// A send-only endpoint to report on.
    pub const REPORT: usize = 0;
    /// The device's configuration space.
    pub const DEVICE: usize = 1;
    /// Its one BAR.
    pub const BAR: usize = 2;
    /// Its interrupt.
    pub const IRQ: usize = 3;
    /// A send-only endpoint to the log.
    pub const LOG: usize = 4;

    /// Identity read from configuration space, the status of trying to
    /// rewrite BAR 0, and the status of turning decoding on.
    pub const LABEL_CONFIG: u64 = 20;
    /// The identification register, the liveness answer, and the length and
    /// address of the mapping.
    pub const LABEL_MEMORY: u64 = 21;
    /// A raised interrupt: status of the wait, the count, the device's
    /// interrupt status register, and who the message came from.
    pub const LABEL_RAISED: u64 = 22;
    /// An interrupt on completed work: status, count, the factorial, and the
    /// interrupt status register.
    pub const LABEL_WORKED: u64 = 23;

    pub const VENDOR: u16 = 0x1234;
    pub const DEVICE_ID: u16 = 0x11e8;
    /// What the identification register reads: version 1.0, then "ed".
    pub const IDENT: u32 = 0x0100_00ed;
    /// A number to write to the liveness register, which answers with every
    /// bit turned over.
    pub const PROBE: u32 = 0x1234_5678;
    /// What to ask the factorial of, and the answer.
    pub const INPUT: u32 = 10;
    pub const FACTORIAL: u32 = 3_628_800;
}

/// The boot-time check that a large file reaches one task intact.
pub mod blob {
    /// A send-only endpoint to report on.
    pub const REPORT: usize = 0;
    /// The blob, read-only.
    pub const BLOB: usize = 1;
    pub const LOG: usize = 2;

    /// Status of the mapping, its length, its FNV-1a hash, and whether its
    /// first and last records read as they should.
    pub const LABEL_READ: u64 = 30;
    /// Status of trying to have its physical address.
    pub const LABEL_PHYSICAL: u64 = 31;

    /// The test file: records of sixteen bytes, each its own number in
    /// fifteen decimal digits and a newline, `RECORDS` of them. Every page is
    /// different from every other, so a page out of place changes the hash.
    pub const RECORD: usize = 16;
    pub const RECORDS: usize = 2 * 1024 * 1024;

    /// FNV-1a, 64-bit: simple enough to compute the same way on both sides
    /// of the boundary without sharing code that could be wrong twice.
    pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
}

/// The conversation between the kernel's compositor and a display driver.
///
/// The driver is a server. The kernel calls it with commands to draw and
/// rectangles to show, written into memory the two share; the driver draws
/// them with the software renderer and has its device show the result. To
/// the compositor it is a `Renderer` like any other.
pub mod gpu {
    /// Where the driver's capabilities are.
    ///
    /// The endpoint the kernel calls, receive-only.
    pub const SERVE: usize = 0;
    pub const LOG: usize = 1;
    pub const DEVICE: usize = 2;
    pub const IRQ: usize = 3;
    /// Memory for queues and requests: [`QUEUE_PAGES`] pages.
    pub const QUEUE: usize = 4;
    /// Memory for the pixels the device scans out.
    pub const SCREEN: usize = 5;
    /// Memory the kernel writes commands into: [`SHARED_PAGES`] pages.
    pub const SHARED: usize = 6;
    /// Memory for the mask atlas.
    pub const ATLAS: usize = 7;
    /// The device's memory BARs: BAR `n` is at `BARS + n`, if it has one.
    pub const BARS: usize = 8;
    /// Where the capability to answer a call lands.
    pub const REPLY: usize = 14;
    /// Where the `n`th surface the kernel hands over lands.
    pub const SURFACES: usize = 16;

    pub const QUEUE_PAGES: usize = 16;
    pub const SHARED_PAGES: usize = 64;
    /// Enough for 1920 by 1080.
    pub const SCREEN_PAGES: usize = 2048;
    pub const ATLAS_WIDTH: u32 = 1024;
    pub const ATLAS_HEIGHT: u32 = 256;

    /// Labels of the kernel's calls. The answer's label is zero for done,
    /// or an error code.
    ///
    /// Set up the device. Answered with width, height, pitch, and how many
    /// displays the device has.
    pub const INIT: u64 = 1;
    /// A surface is attached to this call; word 0 is its number.
    pub const SURFACE: u64 = 2;
    /// Word 0 command records are in shared memory, in the format of
    /// `corrode_render::wire`.
    pub const SUBMIT: u64 = 3;
    /// Coverage for the mask atlas is in shared memory; the words are the
    /// rectangle it goes in: x, y, width, height.
    pub const UPLOAD: u64 = 4;
    /// Word 0 rectangles are in shared memory: show them.
    pub const PRESENT: u64 = 5;

    /// The PCI identity of a modern virtio GPU.
    pub const VIRTIO_VENDOR: u16 = 0x1af4;
    pub const VIRTIO_GPU: u16 = 0x1050;
}
