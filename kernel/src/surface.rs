//! Surfaces: pixels a task draws and the compositor shows.
//!
//! A window's contents are too big to pass in registers and the syscall
//! boundary takes no pointers, so they are shared instead. A surface is a run
//! of physical frames mapped twice: read-only in the kernel half, where the
//! renderer reads them, and in the lower half of whichever task holds a
//! capability to it, where that task draws. Nothing is copied on the way to
//! the screen but the pixels themselves, once.
//!
//! # What the client says, and what it cannot
//!
//! Two things cross the boundary, both by value in registers:
//!
//! * [`map`] -- "put this surface in my address space". The kernel chooses
//!   the address and reports it with the size. The task never names an
//!   address, so there is nothing to validate.
//! * [`commit`] -- "I have redrawn this rectangle". The compositor repaints
//!   that much, when it next paints.
//!
//! A commit is advice about *when* to look, not *what* is there. The
//! rectangle is clipped to the surface before anything uses it, and a task
//! that lies about it gets a window that is out of date, which is its own
//! problem. The pixels are the client's to scribble on at any time; the
//! compositor reads whatever is there and never trusts it to mean anything.
//!
//! # What is not here yet
//!
//! Surfaces are created by the kernel at boot and never destroyed. A client
//! draws into the same memory the compositor reads, so a repaint can catch a
//! frame half drawn; the cure is two buffers and a swap on commit.

use alloc::vec::Vec;

use corrode_render::software::SURFACES;
use corrode_render::Rect;

use crate::arch::x86_64::{self, without_interrupts};
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::mm::{frame, heap, paging};
use crate::sync::SpinLock;
use crate::task;

const PAGE: u64 = 4096;

/// Address space set aside for each surface, in the kernel and in a task
/// alike. Also the most a surface can hold: 4096 by 4096 pixels.
const SPAN: u64 = 64 << 20;

/// Where surfaces are mapped in the kernel half: two gigabytes above the
/// heap and one above the back buffer, in the same PML4 slot, so every
/// address space sees them.
const KERNEL_BASE: u64 = heap::HEAP_START as u64 + (2 << 30);

/// Where surfaces are mapped in a task. Above the code and the stack, with
/// room under it for both to grow.
pub const USER_BASE: u64 = 0x0000_0000_4000_0000;

/// Frames that must be left over after a surface has taken its share.
const RESERVE: usize = 1024;

const NOWHERE: Rect = Rect::new(0, 0, 0, 0);

struct Surface {
    width: u32,
    height: u32,
    /// Physical frames, in order. Empty until the surface is ready.
    frames: Vec<u64>,
    /// What the client has committed since the compositor last asked.
    redrawn: Rect,
}

/// Every surface there is, by number. The number is also what the renderer
/// knows the surface by.
///
/// Only ever locked with interrupts off: a commit arrives through a system
/// call, and must not find the lock held by the painter it preempted.
static TABLE: SpinLock<[Option<Surface>; SURFACES]> = SpinLock::new([const { None }; SURFACES]);

fn with<R>(index: usize, f: impl FnOnce(&mut Surface) -> R) -> Option<R> {
    without_interrupts(|| TABLE.lock().get_mut(index)?.as_mut().map(f))
}

fn kernel_address(index: usize) -> u64 {
    KERNEL_BASE + index as u64 * SPAN
}

/// Where a surface goes in the lower half of any task that maps it.
pub fn user_address(index: usize) -> u64 {
    USER_BASE + index as u64 * SPAN
}

/// Make a surface of `width` by `height` black pixels and return its number.
///
/// `None` if there are none left, it is too big, or memory is short. This
/// creates an object and no authority over it: that is a capability, and
/// minting one is the caller's decision.
pub fn create(width: u32, height: u32) -> Option<usize> {
    let bytes = width as u64 * height as u64 * 4;
    if bytes == 0 || bytes > SPAN {
        return None;
    }
    let pages = bytes.div_ceil(PAGE) as usize;

    // Checked up front, as for the back buffer: running out half way would
    // strand the frames already taken.
    let (free, _) = frame::stats();
    if free < pages + pages / 512 + 2 + RESERVE {
        return None;
    }

    // Claim a number before mapping anything at the address it implies. If
    // what follows fails the number stays claimed, with no frames behind it
    // and no capability to it: pages may already be mapped there, and a
    // later surface must not be mapped over them.
    let index = without_interrupts(|| {
        let mut table = TABLE.lock();
        let index = table.iter().position(Option::is_none)?;
        table[index] = Some(Surface {
            width,
            height,
            frames: Vec::new(),
            redrawn: NOWHERE,
        });
        Some(index)
    })?;

    let mut frames = Vec::new();
    frames.try_reserve_exact(pages).ok()?;
    for page in 0..pages as u64 {
        let frame = frame::alloc()?;
        let mapped = paging::with_kernel_mapper(|mapper| {
            // Wiped before anything can see it: a frame off the free list
            // holds whatever its last owner left, and these are about to be
            // readable from ring 3.
            //
            // SAFETY: the frame is fresh from the allocator, so it is ours
            // alone and covered by the direct map.
            unsafe { mapper.zero_frame(frame.addr()) };
            // Read-only: the kernel looks at a client's pixels and has no
            // business changing them.
            mapper.map_4k(
                kernel_address(index) + page * PAGE,
                frame.addr(),
                paging::NO_EXECUTE | paging::GLOBAL,
            )
        });
        if mapped != Some(true) {
            return None;
        }
        frames.push(frame.addr());
    }

    with(index, |surface| surface.frames = frames)?;
    Some(index)
}

/// Where the kernel reads a surface, and its size in pixels: what the
/// renderer needs to be shown it.
pub fn pixels(index: usize) -> Option<(*const u32, u32, u32)> {
    with(index, |surface| {
        (!surface.frames.is_empty()).then_some((
            kernel_address(index) as *const u32,
            surface.width,
            surface.height,
        ))
    })?
}

/// What the client has committed since this was last asked, if anything.
pub fn take_redrawn(index: usize) -> Option<Rect> {
    let redrawn = with(index, |surface| {
        core::mem::replace(&mut surface.redrawn, NOWHERE)
    })?;
    (!redrawn.is_empty()).then_some(redrawn)
}

/// The physical frame behind one page of a surface.
pub fn frame_of(index: usize, page: usize) -> Option<u64> {
    with(index, |surface| surface.frames.get(page).copied())?
}

/// Where a surface is in the task that asked, and how to address it.
pub struct Mapped {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub pitch: u32,
}

/// Map the surface in `slot` into the calling task. Requires READ; the
/// mapping is writable only if the capability also carries WRITE.
///
/// The address is the kernel's choice and depends only on which surface it
/// is, so mapping twice is harmless and gives the same answer. Doing so
/// through a capability with different rights changes what the mapping
/// allows to match.
pub fn map(slot: CapPtr) -> Result<Mapped, CapError> {
    let (index, rights) = cap::surface_of(slot, Rights::READ)?;
    let current = task::current_id().ok_or(CapError::NoSuchTask)?;
    // A kernel task has no lower half to put it in.
    let mut space = task::space_of(current).ok_or(CapError::NoSpace)?;

    // Never executable, whatever else: W^X holds for shared memory too.
    let mut flags = paging::USER | paging::NO_EXECUTE;
    if rights.contains(Rights::WRITE) {
        flags |= paging::WRITABLE;
    }
    let address = user_address(index);

    with(index, |surface| {
        for (page, frame) in surface.frames.iter().enumerate() {
            let va = address + page as u64 * PAGE;
            if !space.map_4k(va, *frame, flags) {
                return Err(CapError::NoSpace);
            }
            // The entry may have been there already with other permissions.
            // This space is installed on this CPU and no other, so one flush
            // here is all of them.
            x86_64::flush_page(va);
        }
        Ok(Mapped {
            address,
            width: surface.width,
            height: surface.height,
            pitch: surface.width * 4,
        })
    })
    .ok_or(CapError::Stale)?
}

/// Tell the compositor that `rect` of the surface in `slot` has been
/// redrawn. Requires WRITE: only whoever can draw can claim to have.
///
/// Costs a lock and a few compares, and wakes nobody. The compositor finds
/// out when it next paints.
pub fn commit(slot: CapPtr, rect: Rect) -> Result<(), CapError> {
    let (index, _) = cap::surface_of(slot, Rights::WRITE)?;
    with(index, |surface| {
        let bounds = Rect::new(0, 0, surface.width, surface.height);
        if let Some(inside) = rect.intersect(&bounds) {
            surface.redrawn = surface.redrawn.union(&inside);
        }
    })
    .ok_or(CapError::Stale)
}
