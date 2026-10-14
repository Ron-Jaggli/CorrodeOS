//! Capabilities.
//!
//! A task can act on a kernel object only if it holds a capability for that
//! object. There is no root, no uid, no "is this the kernel asking" check. Every
//! operation here answers *what does the caller hold*, never *who is the
//! caller*.
//!
//! A capability is an unforgeable reference (`Arc<Object>`) plus a set of
//! rights. Tasks never see the reference: they name capabilities by slot index
//! into their own capability space, so a task cannot even describe an object it
//! has not been given.
//!
//! # The one invariant
//!
//! **Rights only ever narrow.** [`Rights`] deliberately has no `union`,
//! `insert`, or `with` operation -- there is no code path anywhere that adds a
//! right to a capability, so amplification is not prevented by a check that
//! could be forgotten, it is unrepresentable. Derivation takes a mask and
//! intersects; asking for more than you hold silently gets you less, and the
//! caller is told what it actually received.
//!
//! # Where authority comes from
//!
//! [`mint`] creates authority over an existing object out of nothing, and is the
//! single place in the kernel that can. It is called once during boot to seed
//! the first task. Every other capability over an object descends from that one
//! by copying and narrowing. If `mint` ever acquires a second caller outside
//! boot, the security argument is gone.
//!
//! [`reply_capability`] is the one other constructor, and it is deliberately
//! *not* a second `mint`. It confers no authority over any object: it authorises
//! unblocking one specific caller, once, and it exists only because that caller
//! asked to be answered. The caller is the source of that authority, the same
//! way a question is the source of the right to answer it. It expires on use.

use alloc::sync::Arc;
use alloc::vec::Vec;

use corrode_abi::kind;

use crate::task;

/// How a task names a capability: an index into its own capability space.
pub type CapPtr = usize;

/// Rights carried by a capability.
///
/// There is intentionally no operation that adds a right. `intersect` is the
/// only combinator, which makes amplification unrepresentable rather than
/// merely forbidden.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1 << 0);
    pub const WRITE: Rights = Rights(1 << 1);
    /// Permission to hand this capability to another task.
    pub const GRANT: Rights = Rights(1 << 2);

    pub const READ_WRITE: Rights = Rights(0b011);
    /// May use, and may pass on. The combination a task needs to be able to
    /// hand a capability to somebody else.
    pub const READ_GRANT: Rights = Rights(0b101);
    pub const ALL: Rights = Rights(0b111);

    /// Interpret raw bits as a **mask**, for a narrowing operation.
    ///
    /// This does not breach the one invariant, and it is worth being precise
    /// about why. A mask is not a rights set a task holds; it is an argument to
    /// [`Capability::derive`], which intersects. A caller passing every bit set
    /// still receives exactly what it already had. There is deliberately no
    /// inverse -- no way to turn bits into the rights *on* a capability -- so
    /// the only thing a fabricated value here can do is ask for too much and be
    /// given less.
    ///
    /// Unknown bits are dropped rather than rejected, so bits that gain meaning
    /// later cannot be requested by a program compiled before they existed.
    pub const fn from_mask_bits(bits: u32) -> Rights {
        Rights(bits & Rights::ALL.0)
    }

    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// The only way to combine rights: keep what both sides have.
    pub const fn intersect(self, other: Rights) -> Rights {
        Rights(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bits, for reporting across the syscall boundary.
    ///
    /// Read-only on purpose: there is no constructor from bits, so this cannot
    /// become a way to build a rights set that was never derived from one.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl core::fmt::Debug for Rights {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let r = if self.contains(Rights::READ) {
            'r'
        } else {
            '-'
        };
        let w = if self.contains(Rights::WRITE) {
            'w'
        } else {
            '-'
        };
        let g = if self.contains(Rights::GRANT) {
            'g'
        } else {
            '-'
        };
        write!(f, "{r}{w}{g}")
    }
}

/// A kernel object a capability can point at.
pub enum Object {
    /// The serial console. Exists specifically to make the point: printing is
    /// ambient authority in most kernels, and here it is not.
    Console,
    /// A physical frame, by address.
    ///
    /// Dropping the last capability to a frame does not yet return it to the
    /// allocator: reclaiming safely means knowing nothing still maps it, which
    /// needs mapping metadata this kernel does not keep. Recorded here rather
    /// than papered over with a `Drop` that would be wrong in general.
    Frame(u64),
    /// Another task.
    Task(usize),
    /// An IPC rendezvous point. WRITE means may send, READ means may receive,
    /// and the two being separable is what stops a client impersonating the
    /// server it talks to.
    Endpoint(crate::ipc::Endpoint),
    /// The right to answer one blocked caller, once. WRITE means may reply.
    ///
    /// Unlike every other object here this one is not a thing in the machine --
    /// it is a *debt*. It names a caller that is parked waiting, and it stops
    /// meaning anything the moment that caller is released.
    Reply(crate::ipc::ReplyToken),
    /// A block of pixels the compositor shows: a window's contents, by its
    /// number in [`crate::surface`]. READ means may map it and look, WRITE
    /// means may draw in it and say so.
    ///
    /// This is how bulk data crosses the ring boundary without a pointer
    /// crossing it. A task does not tell the kernel where its pixels are; it
    /// holds this, and the kernel tells the task.
    Surface(usize),
    /// One function on the PCI bus, by its place in [`crate::pci`]'s table:
    /// its configuration space. READ means may read its registers, WRITE
    /// means may write the ones that are a driver's to write.
    ///
    /// Holding this is the only way to learn the device exists. There is no
    /// call that takes a bus address.
    PciDevice(usize),
    /// A run of a device's memory -- one BAR -- by its place in the same
    /// table. It is a separate object from the device on purpose: a task can
    /// be given a device's registers without its configuration space, or the
    /// other way about.
    DeviceMemory(usize),
    /// A device's interrupt, by its line in [`crate::irq`]. READ means may
    /// wait for it. There is nothing to write: the sender is the hardware.
    Interrupt(usize),
    /// A run of frames, by its number in [`crate::memory`]. READ means may
    /// map it, WRITE may write it -- and may ask where it physically is,
    /// which is what a device is told.
    Memory(usize),
    /// A file the bootloader loaded, by its place in [`crate::modules`].
    /// READ means may map it and read it. Nothing means may change it.
    Blob(usize),
}

impl core::fmt::Debug for Object {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Object::Console => write!(f, "Console"),
            Object::Frame(address) => write!(f, "Frame({address:#x})"),
            Object::Task(id) => write!(f, "Task({id})"),
            Object::Endpoint(_) => write!(f, "Endpoint"),
            Object::Reply(token) => write!(f, "Reply(task {})", token.caller()),
            Object::Surface(index) => write!(f, "Surface({index})"),
            Object::PciDevice(index) => write!(f, "PciDevice({index})"),
            Object::DeviceMemory(index) => write!(f, "DeviceMemory({index})"),
            Object::Interrupt(index) => write!(f, "Interrupt({index})"),
            Object::Memory(index) => write!(f, "Memory({index})"),
            Object::Blob(index) => write!(f, "Blob({index})"),
        }
    }
}

/// Discriminants are explicit and stable: a ring 3 task reads these out of
/// `SYS_IDENTIFY`, so reordering the variants would silently change what a
/// compiled user program sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectKind {
    Console = kind::CONSOLE,
    Frame = kind::FRAME,
    Task = kind::TASK,
    Endpoint = kind::ENDPOINT,
    Reply = kind::REPLY,
    Surface = kind::SURFACE,
    PciDevice = kind::PCI_DEVICE,
    DeviceMemory = kind::DEVICE_MEMORY,
    Interrupt = kind::INTERRUPT,
    Memory = kind::MEMORY,
    Blob = kind::BLOB,
}

impl Object {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Console => ObjectKind::Console,
            Object::Frame(_) => ObjectKind::Frame,
            Object::Task(_) => ObjectKind::Task,
            Object::Endpoint(_) => ObjectKind::Endpoint,
            Object::Reply(_) => ObjectKind::Reply,
            Object::Surface(_) => ObjectKind::Surface,
            Object::PciDevice(_) => ObjectKind::PciDevice,
            Object::DeviceMemory(_) => ObjectKind::DeviceMemory,
            Object::Interrupt(_) => ObjectKind::Interrupt,
            Object::Memory(_) => ObjectKind::Memory,
            Object::Blob(_) => ObjectKind::Blob,
        }
    }
}

#[derive(Clone)]
pub struct Capability {
    object: Arc<Object>,
    rights: Rights,
}

impl Capability {
    pub fn rights(&self) -> Rights {
        self.rights
    }

    pub fn kind(&self) -> ObjectKind {
        self.object.kind()
    }

    /// Produce a weaker capability on the same object.
    ///
    /// The result's rights are `self.rights & mask`, always. There is no input
    /// for which this returns more than it was called on.
    pub fn derive(&self, mask: Rights) -> Capability {
        Capability {
            object: Arc::clone(&self.object),
            rights: self.rights.intersect(mask),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// No capability in that slot.
    Empty,
    /// Slot index past the end of the capability space.
    OutOfRange,
    /// Slot already holds a capability.
    Occupied,
    /// Held, but without the rights this operation needs.
    InsufficientRights,
    /// Held, but pointing at the wrong kind of object.
    WrongType,
    /// No such task, or no scheduler yet.
    NoSuchTask,
    /// A reply capability whose call is over: the caller was already answered,
    /// or has since made a different call. Not a rights failure -- the holder
    /// had the authority, it simply expired.
    Stale,
    /// A call could not be set up, because the receiver has nowhere to put the
    /// capability that would answer it. Both sides are told: the caller is
    /// never parked on an answer that could not come, and the receiver never
    /// silently swallows a question it cannot reply to.
    Refused,
    /// Nowhere to put a mapping: the caller has no address space of its own,
    /// or there is no memory left for the page tables.
    NoSpace,
    /// The capability is good and the request is not: an offset past the end
    /// of the object, a width that does not exist, a register the kernel
    /// keeps to itself. Distinct from `InsufficientRights` because no right
    /// would have made it legal.
    Invalid,
}

impl CapError {
    /// Stable number for this error on the far side of a syscall.
    ///
    /// Written out rather than derived from the enum's discriminant: a user
    /// program compiled against these numbers must not be broken by someone
    /// reordering the variants, and zero is reserved for success so a status
    /// register reads the obvious way. The numbers are in `corrode-abi`,
    /// where a ring 3 program reads the same ones.
    pub const fn code(self) -> u64 {
        use corrode_abi::error;
        match self {
            CapError::Empty => error::EMPTY,
            CapError::OutOfRange => error::OUT_OF_RANGE,
            CapError::Occupied => error::OCCUPIED,
            CapError::InsufficientRights => error::INSUFFICIENT_RIGHTS,
            CapError::WrongType => error::WRONG_TYPE,
            CapError::NoSuchTask => error::NO_SUCH_TASK,
            CapError::Stale => error::STALE,
            CapError::Refused => error::REFUSED,
            CapError::NoSpace => error::NO_SPACE,
            CapError::Invalid => error::INVALID,
        }
    }
}

/// A task's capability space: a flat table of slots.
///
/// Flat rather than a radix tree because a few dozen slots is plenty for now
/// and a tree would be structure without a purpose. The slot index is the only name a
/// task has for an object.
pub struct CSpace {
    slots: Vec<Option<Capability>>,
}

impl CSpace {
    pub fn new(slots: usize) -> Self {
        let mut table = Vec::new();
        table.resize_with(slots, || None);
        Self { slots: table }
    }

    pub fn get(&self, slot: CapPtr) -> Result<&Capability, CapError> {
        self.slots
            .get(slot)
            .ok_or(CapError::OutOfRange)?
            .as_ref()
            .ok_or(CapError::Empty)
    }

    pub fn put(&mut self, slot: CapPtr, cap: Capability) -> Result<(), CapError> {
        let entry = self.slots.get_mut(slot).ok_or(CapError::OutOfRange)?;
        if entry.is_some() {
            return Err(CapError::Occupied);
        }
        *entry = Some(cap);
        Ok(())
    }

    pub fn take(&mut self, slot: CapPtr) -> Result<Capability, CapError> {
        self.slots
            .get_mut(slot)
            .ok_or(CapError::OutOfRange)?
            .take()
            .ok_or(CapError::Empty)
    }

    pub fn occupied(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

/// Create authority from nothing and place it in `task`'s capability space.
///
/// # The bootstrap hole
/// Every capability system needs one. This is ours, and it should have no
/// callers outside boot-time seeding. It is not `unsafe` in the Rust sense --
/// it cannot cause undefined behaviour -- but it is the one function whose
/// misuse silently removes the property the rest of this module exists to
/// provide. Everything a task holds at runtime should descend from a boot-time
/// mint by copying and narrowing, never by minting afresh.
pub fn mint(task_id: usize, slot: CapPtr, object: Object, rights: Rights) -> Result<(), CapError> {
    let cap = Capability {
        object: Arc::new(object),
        rights,
    };
    task::with_cspace(task_id, |cspace| cspace.put(slot, cap)).ok_or(CapError::NoSuchTask)?
}

/// Build the one-shot capability that answers a blocked caller.
///
/// # Why this is not a second [`mint`]
/// `mint` is dangerous because it fabricates authority over an object that
/// already exists and has other holders. This fabricates nothing of the kind.
/// The token names a caller that is, right now, parked waiting for an answer,
/// and the only thing the capability permits is giving it one. A server that
/// holds it can unblock that caller and nothing else: it cannot read the
/// caller's memory, stop it, or name any object the caller holds.
///
/// It is issued with `WRITE` alone -- may reply -- and deliberately without
/// `GRANT`, so a server cannot hand the right to answer its client to a third
/// party. Copying it inside the server's own space is possible and useless: the
/// token carries the call's sequence number, and every copy goes stale together
/// the moment the call is answered.
pub fn reply_capability(token: crate::ipc::ReplyToken) -> Capability {
    Capability {
        object: Arc::new(Object::Reply(token)),
        rights: Rights::WRITE,
    }
}

/// The caller a Reply capability answers. Requires WRITE -- the right to reply.
pub fn reply_token(slot: CapPtr) -> Result<crate::ipc::ReplyToken, CapError> {
    let object = lookup(slot, Rights::WRITE)?;
    match *object {
        Object::Reply(token) => Ok(token),
        _ => Err(CapError::WrongType),
    }
}

/// Resolve a slot in the *calling task's* capability space, checking rights.
///
/// This is the chokepoint every operation goes through. Note what it does not
/// take: any notion of who the caller is beyond which capability space is
/// theirs.
pub fn lookup(slot: CapPtr, required: Rights) -> Result<Arc<Object>, CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        if !cap.rights.contains(required) {
            return Err(CapError::InsufficientRights);
        }
        Ok(Arc::clone(&cap.object))
    })
    .ok_or(CapError::NoSuchTask)?
}

/// What is in a slot, without needing any rights to ask.
pub fn identify(slot: CapPtr) -> Result<(ObjectKind, Rights), CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        Ok((cap.kind(), cap.rights()))
    })
    .ok_or(CapError::NoSuchTask)?
}

/// Copy a capability within the calling task's own space, narrowing by `mask`.
///
/// Returns the rights actually granted, which may be less than requested and
/// can never be more.
pub fn copy(src: CapPtr, dst: CapPtr, mask: Rights) -> Result<Rights, CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        // No GRANT needed: the task already holds this authority, and a weaker
        // copy in its own hand adds nothing it could not already do.
        let derived = cspace.get(src)?.derive(mask);
        let rights = derived.rights();
        cspace.put(dst, derived)?;
        Ok(rights)
    })
    .ok_or(CapError::NoSuchTask)?
}

/// Hand a narrowed copy to another task. Requires [`Rights::GRANT`].
///
/// This is the operation that moves authority between protection domains, so it
/// is the one that needs a right of its own. Without GRANT a task can use a
/// capability but never spread it.
pub fn grant(target: usize, src: CapPtr, dst: CapPtr, mask: Rights) -> Result<Rights, CapError> {
    // Derive first, against our own space, so the rights check happens before
    // anything is placed anywhere.
    let derived = derive_for_transfer(src, mask)?;
    let rights = derived.rights();
    task::with_cspace(target, |cspace| cspace.put(dst, derived)).ok_or(CapError::NoSuchTask)??;
    Ok(rights)
}

/// Drop a capability. The object goes away when its last capability does.
pub fn delete(slot: CapPtr) -> Result<(), CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| cspace.take(slot).map(|_| ()))
        .ok_or(CapError::NoSuchTask)?
}

/// Derive a capability for handing to another protection domain.
///
/// Requires `GRANT`, because that is what this is: giving authority away. Used
/// by both [`grant`] and IPC capability transfer, so the rule is stated once.
pub fn derive_for_transfer(slot: CapPtr, mask: Rights) -> Result<Capability, CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        if !cap.rights().contains(Rights::GRANT) {
            return Err(CapError::InsufficientRights);
        }
        Ok(cap.derive(mask))
    })
    .ok_or(CapError::NoSuchTask)?
}

/// The physical address behind a Frame capability. Requires READ.
///
/// Note the shape: a task cannot ask "what is at physical address X", only
/// "what address does the capability I hold in this slot refer to". The address
/// is a consequence of holding the capability, never a way to obtain one.
pub fn frame_address(slot: CapPtr) -> Result<u64, CapError> {
    let object = lookup(slot, Rights::READ)?;
    match *object {
        Object::Frame(address) => Ok(address),
        _ => Err(CapError::WrongType),
    }
}

/// The surface a Surface capability names, and the rights it is held with.
/// Requires `required`.
///
/// The rights come back because what a mapping allows depends on them: the
/// same call gives a holder of WRITE somewhere to draw and a holder of READ
/// alone somewhere to look.
pub fn surface_of(slot: CapPtr, required: Rights) -> Result<(usize, Rights), CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        if !cap.rights.contains(required) {
            return Err(CapError::InsufficientRights);
        }
        match *cap.object {
            Object::Surface(index) => Ok((index, cap.rights)),
            _ => Err(CapError::WrongType),
        }
    })
    .ok_or(CapError::NoSuchTask)?
}

/// The device a PciDevice capability names. Requires `required`.
pub fn pci_device_of(slot: CapPtr, required: Rights) -> Result<usize, CapError> {
    let object = lookup(slot, required)?;
    match *object {
        Object::PciDevice(index) => Ok(index),
        _ => Err(CapError::WrongType),
    }
}

/// The region a DeviceMemory capability names, and the rights it is held
/// with. Requires `required`.
pub fn device_memory_of(slot: CapPtr, required: Rights) -> Result<(usize, Rights), CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        if !cap.rights.contains(required) {
            return Err(CapError::InsufficientRights);
        }
        match *cap.object {
            Object::DeviceMemory(index) => Ok((index, cap.rights)),
            _ => Err(CapError::WrongType),
        }
    })
    .ok_or(CapError::NoSuchTask)?
}

/// The object a Memory capability names, and the rights it is held with.
/// Requires `required`.
pub fn memory_of(slot: CapPtr, required: Rights) -> Result<(usize, Rights), CapError> {
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    task::with_cspace(current, |cspace| {
        let cap = cspace.get(slot)?;
        if !cap.rights.contains(required) {
            return Err(CapError::InsufficientRights);
        }
        match *cap.object {
            Object::Memory(index) => Ok((index, cap.rights)),
            _ => Err(CapError::WrongType),
        }
    })
    .ok_or(CapError::NoSuchTask)?
}

/// The module a Blob capability names. Requires `required`.
pub fn blob_of(slot: CapPtr, required: Rights) -> Result<usize, CapError> {
    let object = lookup(slot, required)?;
    match *object {
        Object::Blob(index) => Ok(index),
        _ => Err(CapError::WrongType),
    }
}

/// The line an Interrupt capability names. Requires `required`.
pub fn interrupt_of(slot: CapPtr, required: Rights) -> Result<usize, CapError> {
    let object = lookup(slot, required)?;
    match *object {
        Object::Interrupt(index) => Ok(index),
        _ => Err(CapError::WrongType),
    }
}

/// The task a Task capability names. Requires READ.
pub fn task_of(slot: CapPtr) -> Result<usize, CapError> {
    let object = lookup(slot, Rights::READ)?;
    match *object {
        Object::Task(id) => Ok(id),
        _ => Err(CapError::WrongType),
    }
}

/// How many capabilities a task currently holds.
pub fn slots_used(task_id: usize) -> Option<usize> {
    task::with_cspace(task_id, |cspace| cspace.occupied())
}

/// Write to the console -- if the caller holds a console capability with WRITE.
///
/// The point of this function is that it is the *only* way a task can print.
/// The kernel's own boot logging still goes straight to the serial port, which
/// is ambient authority; converting that is not the goal, because the kernel is
/// not a protection domain. Tasks are, and a task with no console capability
/// has no way to produce output at all.
pub fn console_write(slot: CapPtr, message: &str) -> Result<(), CapError> {
    let object = lookup(slot, Rights::WRITE)?;
    if object.kind() != ObjectKind::Console {
        return Err(CapError::WrongType);
    }
    crate::print!("{}", message);
    Ok(())
}
