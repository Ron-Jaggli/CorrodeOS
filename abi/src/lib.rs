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
/// Read an I/O port in a port-range capability. Needs READ. `rsi` is the
/// offset into the range and `rdx` the width in bytes: 1, 2 or 4. The value
/// comes back in `rdx`.
pub const SYS_PORT_IN: u64 = 14;
/// Write one. Needs WRITE. `rsi` is the offset, `rdx` the width, `r10` the
/// value.
pub const SYS_PORT_OUT: u64 = 15;
/// The monotonic clock: nanoseconds since boot, in `rdx`. Takes nothing and
/// needs no capability -- the timestamp counter is readable in ring 3
/// anyway, so this hands out nothing a program could not already measure.
pub const SYS_CLOCK: u64 = 16;
/// Sleep until the monotonic clock reads `rdi` nanoseconds. Returns at once
/// if it already has. A deadline rather than a duration, so a program that
/// sleeps in a loop does not drift by however late each wake-up was.
pub const SYS_SLEEP: u64 = 17;
/// Make `rsi` pages of fresh, zeroed memory out of the pool capability in
/// `rdi`, and put a capability to it in the caller's slot `rdx`. Needs WRITE
/// on the pool. The length in bytes comes back in `rdx`; map it with
/// `SYS_MAP`.
pub const SYS_ALLOC: u64 = 18;
/// Wait for the window in `rdi` to have something to say, or for the clock
/// to read `rsi` nanoseconds, whichever is first. Needs READ. The event
/// comes back as a message: label in `rsi`, words in `rdx`, `r10`, `r8`,
/// `r9`. A label of zero means the deadline came first. A deadline of zero
/// looks and returns at once; `u64::MAX` waits for ever.
pub const SYS_WAIT: u64 = 19;
/// Seal a message with the sealer in `rdi`, in the first page of the memory
/// in `rsi`, which must be held READ and WRITE. Needs WRITE on the sealer.
/// `rdx` is the message's length; it is read from byte [`seal::OVERHEAD`]
/// of the page on. The sealed message, [`seal::OVERHEAD`] bytes longer,
/// replaces it from byte 0, and its length comes back in `rdx`.
pub const SYS_SEAL: u64 = 20;
/// Undo `SYS_SEAL`: the sealed message is from byte 0, `rdx` long, and what
/// was sealed comes back from byte [`seal::OVERHEAD`], its length in `rdx`.
/// Needs READ on the sealer. Fails with [`error::FORGED`], and the page's
/// first `rdx` bytes zeroed, if the message was changed or another sealer
/// sealed it.
pub const SYS_UNSEAL: u64 = 21;

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
    /// A sealed message that does not check out: changed, cut short, or
    /// sealed by someone else.
    pub const FORGED: u64 = 11;
    /// The secure core is not there to ask.
    pub const UNAVAILABLE: u64 = 12;
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
    pub const IO_PORTS: u8 = 12;
    pub const POOL: u8 = 13;
    pub const WINDOW: u8 = 14;
    pub const SEALER: u8 = 15;
}

/// Sealed messages, as `SYS_SEAL` writes them: the nonce, the tag, then the
/// ciphertext.
pub mod seal {
    /// Bytes before the message: a 12-byte nonce and a 16-byte tag.
    pub const OVERHEAD: usize = 28;
    /// The longest message one call seals: what the secure core's mailbox
    /// carries.
    pub const MOST: usize = 4032;
}

/// What every desktop program is handed, in the same slots, and what it may
/// put in the others.
pub mod app {
    /// A send-only endpoint to the window server.
    pub const WINDOWS: usize = 0;
    /// A pool of memory to make a heap from.
    pub const POOL: usize = 1;
    /// A send-only endpoint to the log.
    pub const LOG: usize = 2;
    /// Where the program should ask for its window to be put.
    pub const WINDOW: usize = 3;
    /// Where it should ask for its heap to be put.
    pub const HEAP: usize = 4;
    /// For the panel only: a send-only endpoint to the launcher.
    pub const LAUNCHER: usize = 5;
    /// For the terminal only: the files packed from `root/` at build time,
    /// a ustar archive, as a read-only blob.
    pub const FILES: usize = 6;
    /// For Settings, Control Center and the menu only: a send-only endpoint
    /// to the desktop shell, which takes the calls that reach past the
    /// caller's own windows -- the look, other apps' windows, notifications.
    pub const SHELL: usize = 7;
    /// For Settings and the lock screen only: a sealer, with a key of the
    /// program's own that never leaves the secure core.
    pub const SEAL: usize = 8;

    /// Pages in each program's pool.
    pub const POOL_PAGES: u64 = 256;
}

/// The window server: one call to open a window, and the events a window
/// then receives.
pub mod window {
    /// A call: open a window `word 0` by `word 1` pixels, titled with the
    /// up to eight ASCII bytes of word 3, little-endian. Word 2 is 1 for a
    /// panel docked along the top of the screen -- as wide as the screen,
    /// whatever was asked, with no title bar and never covered. The answer
    /// carries the window's capability and says its size in words 0 and 1.
    pub const OPEN: u64 = 60;
    pub const OPENED: u64 = 61;
    /// Flags, in word 2 of `OPEN`.
    pub const DOCKED: u64 = 1;
    /// The top byte of each pixel says how transparent it is: 0 opaque,
    /// 255 not there. A translucent window is drawn over glass where the
    /// desktop has it, so a panel can be a pane rather than a slab.
    pub const TRANSLUCENT: u64 = 2;
    /// An `OPEN` flag: the program can draw at any size, so the desktop
    /// may resize the window -- zoom fills the screen with it -- and says
    /// so with [`RESIZE`].
    pub const RESIZABLE: u64 = 4;
    /// A popover: no title bar, under the right-hand end of the menu bar,
    /// and told to close when anything else is clicked.
    pub const POPOVER: u64 = 8;
    /// A cover: the whole screen, above everything, with every key and
    /// click while it is up. The lock screen is one.
    pub const COVER: u64 = 16;
    /// With POPOVER: a context menu, placed at the pointer where the
    /// desktop was right-clicked instead of under the menu bar.
    pub const MENU: u64 = 32;

    /// A key, while this window has the keyboard. Words as `ps2::LABEL_KEY`.
    pub const KEY: u64 = 62;
    /// A mouse button went down or up inside the window. Words 0 and 1 are
    /// where, from the window's top left corner; word 2 the buttons held.
    pub const BUTTON: u64 = 63;
    /// The window gained the keyboard (word 0 is 1) or lost it (0).
    pub const FOCUS: u64 = 64;
    /// The close button on the window's title bar was clicked. A program
    /// that wants to go should exit; its window goes with it. One that
    /// ignores this keeps its window.
    pub const CLOSE: u64 = 65;
    /// The desktop's look, in the four words `corrode-theme` packs it
    /// into. Sent when a window opens and whenever the look changes.
    pub const THEME: u64 = 66;
    /// To the window server: make this the desktop's look, in the same four
    /// words as [`THEME`]. Every window is told, and the desktop restyles
    /// itself. Answered with word 0 zero.
    pub const SET_THEME: u64 = 67;
    /// To docked windows: the window with the keyboard is now the one
    /// titled word 0 (its eight ASCII bytes, little-endian), or none if 0.
    /// What the menu bar shows as the app in front.
    pub const ACTIVE: u64 = 68;
    /// The window is now word 0 by word 1 pixels, `width * 4` bytes a row,
    /// in the same mapping. Redraw all of it. Only to resizable windows.
    pub const RESIZE: u64 = 69;
    /// To docked windows: the wall clock. Word 0 is seconds since 1970 UTC
    /// at the monotonic nanosecond in word 1; the time after that is that
    /// plus how far the monotonic clock has run.
    pub const CLOCK: u64 = 70;
    /// The wheel turned over a window: word 0 is notches, toward the person
    /// positive, as i64; words 1 and 2 where the pointer is in the window.
    pub const SCROLL: u64 = 71;
    /// To Files: show this file. The path is the four words' 32 bytes,
    /// little-endian, NUL-padded; a longer path is not sent.
    pub const REVEAL: u64 = 72;
    /// A call: show a notification saying up to 32 bytes of ASCII, the four
    /// words little-endian and NUL-padded, under the caller's window title.
    /// The answer's word 0 is 0.
    pub const NOTIFY: u64 = 73;
    /// Ask for a recent notification: word 0 which (0 the newest), word 1
    /// which part. Part 0 answers how many there are, the sender's title
    /// (8 bytes) and how many seconds ago it came; part 1 its text, 32
    /// bytes. Past the last, every word is zero but the count. Part 2
    /// clears the list.
    pub const HISTORY: u64 = 74;
    /// To a popover: the pointer is over it, at x, y from its corner.
    pub const HOVER: u64 = 75;
    /// Ask what was last right-clicked: the kind (0 the desktop, 1 a dock
    /// icon), the launcher's number for its program, whether it is
    /// running, and its window title (8 bytes).
    pub const CONTEXT: u64 = 76;
    /// Close every window titled word 0 (8 bytes): Quit, from the dock.
    /// Answers how many were asked to close.
    pub const QUIT: u64 = 77;
    /// Set what the caller's windows say after their title, as a terminal
    /// names its folder: up to 24 bytes in words 0-2, unused ones zero.
    pub const SUBTITLE: u64 = 78;
    /// The windows titled word 0, for a dock icon's menu: word 1 picks
    /// which. Answers the count, then that one's subtitle in words 1-3.
    pub const LIST: u64 = 79;
    /// Bring the window `LIST` would name (title in word 0, which in word
    /// 1) to the front, and back from the dock if it was minimized.
    pub const RAISE: u64 = 80;
    /// The answer to a call the window server's own endpoint does not take:
    /// one only the shell endpoint does, from a program that lacks it.
    pub const DENIED: u64 = 81;
    /// A call, from the lock screen and only it -- from anyone else the
    /// answer is always no -- is this the passcode? Words 0..4 are its up
    /// to 32 ASCII characters, little-endian, NUL-padded. All four zero
    /// asks only whether there is a passcode at all, which costs no try
    /// (a passcode is never empty). The
    /// answer's word 0 is 1 for yes (or for no passcode set) and 0 for no;
    /// word 1 is how many seconds until another try is taken, after too
    /// many wrong ones.
    pub const UNLOCK: u64 = 82;
    /// A call: make the caller's popover word 0 by word 1 pixels, as
    /// Control Center does when it has more to show. Only a popover, and
    /// only its own: other windows are sized by the person. The answer's
    /// word 0 is 1 if it will be; the window then hears [`RESIZE`].
    pub const GROW: u64 = 83;
    /// To Control Center: a notification was just posted, so the list it
    /// shows is out of date. Nothing in the words; ask again.
    pub const NOTIFIED: u64 = 84;
    /// A call, from Settings and only it: is this, in words 0..4 as for
    /// [`UNLOCK`], the passcode? The first step of changing it, under the
    /// same limit on tries. The answer's word 0 is 1 for yes; word 1 the
    /// seconds to wait after too many wrong tries; word 2 is 1 if there is
    /// no vault, whose passcode is the only one that can be changed (the
    /// boot medium's desktop.conf cannot be written).
    pub const PASSCODE_CHECK: u64 = 85;
    /// A call, from Settings and only it, within a minute of a yes to
    /// [`PASSCODE_CHECK`] from the same task: make words 0..4 the
    /// passcode. The vault is sealed again under it on the disk. The
    /// answer's word 0 is 1 if it was.
    pub const PASSCODE_SET: u64 = 86;
    /// The calls only the shell endpoint takes.
    pub const SHELL_ONLY: [u64; 6] = [SET_THEME, QUIT, LIST, RAISE, HISTORY, CONTEXT];

    /// Most a window may be, either way.
    pub const MAX_WIDTH: u32 = 1920;
    pub const MAX_HEIGHT: u32 = 1200;
}

/// The launcher: start one of the programs the system was booted with.
pub mod launcher {
    /// Start program number word 0. The answer's word 0 is zero for
    /// started, or the status of why not.
    pub const LAUNCH: u64 = 70;
    pub const LAUNCHED: u64 = 71;
    pub const TERMINAL: u64 = 1;
    pub const SETTINGS: u64 = 2;
    pub const ABOUT: u64 = 3;
    pub const FILES: u64 = 4;
    /// Control Center, the menu bar's quick settings.
    pub const CONTROL: u64 = 5;
    /// The lock screen.
    pub const LOCK: u64 = 6;
    /// The desktop's right-click menu.
    pub const MENU: u64 = 7;
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

/// The PS/2 driver and what it tells the compositor.
///
/// The slots are where the kernel puts the driver's capabilities before it
/// runs. Each event is one message on the `EVENTS` endpoint.
pub mod ps2 {
    /// The controller's data port, 0x60, and nothing else.
    pub const DATA: usize = 0;
    /// Its status and command port, 0x64, and nothing else.
    pub const COMMAND: usize = 1;
    /// The keyboard's and the mouse's interrupts, which share one line: both
    /// are read from the same port, so one wait covers both.
    pub const IRQ: usize = 2;
    /// A send-only endpoint to the compositor.
    pub const EVENTS: usize = 3;
    /// A send-only endpoint to the log.
    pub const LOG: usize = 4;

    /// A key went down or came up. Word 0 is the key, as
    /// `corrode_input::Key`; word 1 is 1 for down and 0 for up; word 2 is the
    /// character it types, or zero; word 3 the modifier bits.
    pub const LABEL_KEY: u64 = 50;
    /// The mouse moved or a button changed. Words 0 and 1 are how far right
    /// and down, as signed numbers; word 2 the buttons held.
    pub const LABEL_POINTER: u64 = 51;
    /// The driver is up: word 0 is 1 if a keyboard answered, word 1 is 1 if a
    /// mouse did.
    pub const LABEL_READY: u64 = 52;
    /// A pointer that knows where it is, not how far it went -- a tablet:
    /// words 0 and 1 are the position across and down, each from 0 to
    /// [`POINTER_AT_MAX`] for the far edge; word 2 the buttons held; word 3
    /// the wheel, as for [`LABEL_POINTER`].
    pub const LABEL_POINTER_AT: u64 = 53;
    pub const POINTER_AT_MAX: u64 = 0x7fff;
}

/// Verdigris: Windows programs (see `verdigris/DESIGN.md`).
pub mod verdigris {
    /// Where Verdigris's kernel32 is linked, with its export table at this
    /// very address. A Windows program's image must end below it.
    pub const KERNEL32_BASE: u64 = 0x0800_0000;
    /// A Windows program's first capability: the log, where its standard
    /// output and error go.
    pub const LOG: usize = 0;
    /// Its view of the files, send-only: [`view`](super::view)'s calls.
    /// Absent, the program has no files at all.
    pub const VIEW: usize = 1;
    /// The page it shares with its view, for paths and what is read.
    pub const PAGE: usize = 2;
}

/// A Windows program's view of the files (`verdigris/DESIGN.md` section 4),
/// served by `fileview`, one task per view. The program holds the view's
/// endpoint send-only and one page shared with the service; the service
/// holds the real tree and the list of what the program may see, which
/// the program never holds.
///
/// Every answer is `[error, value, 0, 0]` under the call's own label:
/// `error` is a Windows error code (0 for none), as `GetLastError` gives
/// it; what is not in the view is `ERROR_FILE_NOT_FOUND` (2) or
/// `ERROR_PATH_NOT_FOUND` (3), as for a name that does not exist.
pub mod view {
    /// The endpoint it receives on.
    pub const SERVE: usize = 0;
    /// The real files: the archive, a blob.
    pub const TREE: usize = 1;
    /// The page it shares with the program.
    pub const PAGE: usize = 2;
    /// What the program may see, read-only: its own folder on the first
    /// line, then one allowed path a line, `NUL` after the last.
    pub const SPEC: usize = 3;
    pub const LOG: usize = 4;
    /// Where each call's reply capability goes.
    pub const REPLY: usize = 5;
    /// Memory to buy its heap from, and where the heap's capability goes.
    pub const POOL: usize = 6;
    pub const HEAP: usize = 7;
    pub const HEAP_PAGES: usize = 32;

    /// `[len, write, disposition, 0]`, the path in UTF-8 in the page's
    /// first `len` bytes: open a file, as `CreateFileW`. `write` is 1 if
    /// any right to change it was asked for. Answers the handle.
    pub const OPEN: u64 = 1;
    /// `[handle, len, 0, 0]`: up to `len` bytes (at most a page) from where
    /// the handle is, into the page, and move on. Answers how many; 0 at
    /// the end.
    pub const READ: u64 = 2;
    /// `[handle, 0, 0, 0]`: the file's size in bytes.
    pub const SIZE: u64 = 3;
    /// `[handle, 0, 0, 0]`: close it.
    pub const CLOSE: u64 = 4;
    /// The program is ending, sent, not called: the service ends too.
    pub const DONE: u64 = 5;

    /// Files a program may have open at once.
    pub const HANDLES: usize = 16;
    /// The longest path, in UTF-8 bytes.
    pub const MOST_PATH: usize = 2048;
    /// `ERROR_INVALID_HANDLE`, `ERROR_TOO_MANY_OPEN_FILES`,
    /// `ERROR_FILENAME_EXCED_RANGE`: the service's own refusals.
    pub const INVALID_HANDLE: u64 = 6;
    pub const TOO_MANY_OPEN: u64 = 4;
    pub const NAME_TOO_LONG: u64 = 206;
}

/// The USB host controller's driver, `xhci`: an xHCI controller, the
/// keyboards and pointers on it, and what they say passed on to the
/// compositor in the same messages as [`ps2`]'s.
pub mod xhci {
    /// The controller's configuration space.
    pub const DEVICE: usize = 0;
    /// Its registers, BAR 0.
    pub const BAR: usize = 1;
    /// Its interrupt (MSI-X or MSI message 0, on interrupter 0).
    pub const IRQ: usize = 2;
    /// Memory for its rings, contexts and buffers: [`MEMORY_PAGES`] pages.
    pub const MEMORY: usize = 3;
    /// A send-only endpoint to the compositor: [`ps2`]'s labels.
    pub const EVENTS: usize = 4;
    /// A send-only endpoint to the log.
    pub const LOG: usize = 5;

    pub const MEMORY_PAGES: usize = 32;

    /// The PCI class of an xHCI controller: serial bus, USB, xHCI.
    pub const CLASS: u8 = 0x0c;
    pub const SUBCLASS: u8 = 0x03;
    pub const PROG_IF: u8 = 0x30;
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
/// The settings store: a driver for a virtio block device, which keeps the
/// desktop's theme in the disk's first sector. The desktop is its only
/// client.
pub mod store {
    /// Where the driver's capabilities are.
    ///
    /// The endpoint the desktop calls, receive-only.
    pub const SERVE: usize = 0;
    pub const LOG: usize = 1;
    pub const DEVICE: usize = 2;
    /// Memory for the queue, its requests and one sector:
    /// [`QUEUE_PAGES`] pages.
    pub const QUEUE: usize = 3;
    /// The device's memory BARs: BAR `n` is at `BARS + n`, if it has one.
    pub const BARS: usize = 4;
    /// Where the capability to answer a call lands.
    pub const REPLY: usize = 10;
    pub const QUEUE_PAGES: usize = 3;

    /// The modern (virtio 1) block device's id, which QEMU gives with
    /// `disable-legacy=on`, and the transitional one it gives by default.
    pub const VIRTIO_BLK: u16 = 0x1042;
    pub const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;

    /// Read the kept theme. The answer's words are its four words, and its
    /// label [`crate::STATUS_OK`]; or a status of `Empty` if the disk has
    /// none, or of why it could not be read.
    pub const LOAD: u64 = 90;
    /// Keep the theme in words 0..4. The answer's label is the status.
    pub const SAVE: u64 = 91;
    /// Read the sector numbered in word 0 into the queue memory's sector
    /// page, where the kernel, which made that memory, reads it. The
    /// answer's label is the status. For the vault (`corrode_vault`).
    pub const READ: u64 = 92;
    /// Write the queue memory's sector page, which the kernel filled, to
    /// the sector numbered in word 0. The answer's label is the status.
    pub const WRITE: u64 = 93;
}

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
    /// Memory for the glass's scratch and the wallpaper's pixels:
    /// [`SCRATCH_PAGES`] pages. Optional: without it, glass is a plain veil
    /// and there is no wallpaper.
    pub const SCRATCH: usize = 15;
    /// Where the `n`th surface the kernel hands over lands: one slot for
    /// each of the renderer's surfaces, 16 up to 47.
    pub const SURFACES: usize = 16;

    pub const QUEUE_PAGES: usize = 16;
    pub const SHARED_PAGES: usize = 64;
    /// Enough for 1920 by 1080.
    pub const SCREEN_PAGES: usize = 2048;
    pub const ATLAS_WIDTH: u32 = 1024;
    /// Rows 512 to 767 are the dock's icons drawn at their magnified size,
    /// and the 256 below them more windows' titles.
    pub const ATLAS_HEIGHT: u32 = 1024;
    /// Three screens for the glass, a little for its line buffer, and one
    /// for the wallpaper.
    pub const SCRATCH_PAGES: usize = 4 * SCREEN_PAGES + 4;

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
    /// Paint the wallpaper of the theme in the four words, and show it as
    /// the renderer's last surface. Needs [`SCRATCH`].
    pub const WALLPAPER: u64 = 6;
    /// Stop showing surface word 0 and let go of its capability: its
    /// window has closed, and the kernel will retire it.
    pub const FORGET: u64 = 7;

    /// The PCI identity of a modern virtio GPU.
    pub const VIRTIO_VENDOR: u16 = 0x1af4;
    pub const VIRTIO_GPU: u16 = 0x1050;
}

/// The NVIDIA driver's first stage: look, change nothing.
pub mod nvidia {
    pub const REPORT: usize = 0;
    pub const LOG: usize = 1;
    /// The card's configuration space, READ only.
    pub const DEVICE: usize = 2;
    /// BAR0, its registers, READ only: a read-only capability maps
    /// read-only, so this stage cannot write a register if it tries.
    pub const BAR0: usize = 3;
    /// Memory to copy the VBIOS into: [`ROM_PAGES`] pages.
    pub const ROM: usize = 4;
    /// The firmware files, as blobs, when the boot medium has them.
    pub const BOOTER_LOAD: usize = 5;
    pub const BOOTER_UNLOAD: usize = 6;
    pub const BOOTLOADER: usize = 7;
    pub const GSP: usize = 8;

    pub const ROM_PAGES: usize = 256;

    /// Which firmware files parsed, as bits 0 to 3 in the order of the
    /// slots above; the GSP image's size in KiB; Booter's signature count.
    pub const LABEL_FIRMWARE: u64 = 40;
    /// `NV_PMC_BOOT_0`, `NV_PMC_BOOT_42`, the chipset code, and the usable
    /// framebuffer in MiB.
    pub const LABEL_CARD: u64 = 41;
    /// Bytes of VBIOS read, number of images, BIT version, and the FWSEC
    /// descriptor's offset (all ones if none was found).
    pub const LABEL_VBIOS: u64 = 42;
    /// The driver has finished looking.
    pub const LABEL_DONE: u64 = 43;

    pub const VENDOR: u16 = 0x10de;
}
