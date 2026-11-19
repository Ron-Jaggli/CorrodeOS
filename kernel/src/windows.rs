//! Windows: a surface to draw in, and a queue of what happened to it.
//!
//! A program asks the window server for a window and gets one capability
//! back. With WRITE it maps the window's pixels and commits them, exactly as
//! with a bare surface. With READ it waits on the window for events -- a key
//! while it has the keyboard, a click on it -- or for a deadline, whichever
//! comes first, which is all the waiting a simple program needs: one call
//! that covers "the user did something" and "it is time to redraw the
//! clock".
//!
//! The painter decides everything else: where a window goes, which one is
//! on top, which one has the keyboard. It pushes events in; it never waits
//! for a program to take them. A queue that fills drops what does not fit,
//! so a program that stops listening costs the system nothing but its own
//! missed keys.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use corrode_abi::window;
use corrode_render::Rect;

use crate::arch::x86_64::without_interrupts;
use crate::cap::{self, CapError, CapPtr, Rights};
use crate::surface;
use crate::sync::SpinLock;
use crate::task::{self, State};

/// Events a window holds before it starts dropping them.
const QUEUE: usize = 64;

/// Nobody is waiting.
const NOBODY: usize = usize::MAX;

pub struct Window {
    pub surface: usize,
    pub width: u32,
    pub height: u32,
    pub docked: bool,
    pub translucent: bool,
    pub resizable: bool,
    pub popover: bool,
    pub cover: bool,
    pub menu: bool,
    /// What its title bar says: up to eight bytes, unused ones zero.
    title: [u8; 8],
    /// The task that asked for it. When it finishes, the window goes.
    owner: usize,
    events: VecDeque<(u64, [u64; 4])>,
    waiter: usize,
}

static TABLE: SpinLock<Vec<Window>> = SpinLock::new(Vec::new());

/// Windows made but not yet given a place by the painter.
static NEW: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());

/// The screen's size, so a docked window can be made as wide as it.
static SCREEN: [AtomicU32; 2] = [AtomicU32::new(1024), AtomicU32::new(768)];

pub fn set_screen(width: u32, height: u32) {
    SCREEN[0].store(width, Ordering::Relaxed);
    SCREEN[1].store(height, Ordering::Relaxed);
}

/// Make a window for task `owner` and say its number. `None` if there is
/// no surface left.
pub fn open(width: u32, height: u32, flags: u64, title: u64, owner: usize) -> Option<usize> {
    let docked = flags & window::DOCKED != 0;
    let translucent = flags & window::TRANSLUCENT != 0;
    let cover = flags & window::COVER != 0 && !docked;
    let popover = flags & window::POPOVER != 0 && !docked && !cover;
    let menu = popover && flags & window::MENU != 0;
    let resizable = flags & window::RESIZABLE != 0 && !docked && !popover && !cover;
    let (width, height) = if docked {
        (SCREEN[0].load(Ordering::Relaxed), height.clamp(16, 64))
    } else if cover {
        (
            SCREEN[0].load(Ordering::Relaxed).min(window::MAX_WIDTH),
            SCREEN[1].load(Ordering::Relaxed).min(window::MAX_HEIGHT),
        )
    } else {
        (
            width.clamp(64, window::MAX_WIDTH),
            height.clamp(32, window::MAX_HEIGHT),
        )
    };
    // A window that can be resized gets room for the whole screen: the
    // largest the desktop will make it.
    let room = if resizable {
        SCREEN[0].load(Ordering::Relaxed).min(window::MAX_WIDTH) as u64
            * SCREEN[1].load(Ordering::Relaxed).min(window::MAX_HEIGHT) as u64
    } else {
        0
    };
    let surface = surface::create_with_room(width, height, room)?;
    let index = without_interrupts(|| {
        let mut table = TABLE.lock();
        table.push(Window {
            surface,
            width,
            height,
            docked,
            translucent,
            resizable,
            popover,
            cover,
            menu,
            // Printable ASCII only: a title is drawn, never interpreted.
            title: title
                .to_le_bytes()
                .map(|byte| if (b' '..=b'~').contains(&byte) { byte } else { 0 }),
            owner,
            events: VecDeque::new(),
            waiter: NOBODY,
        });
        table.len() - 1
    });
    without_interrupts(|| NEW.lock().push(index));
    Some(index)
}

/// Windows the painter has not placed yet.
pub fn take_new() -> Vec<usize> {
    without_interrupts(|| core::mem::take(&mut *NEW.lock()))
}

/// A window's surface, size, and whether it is docked.
pub fn shape(index: usize) -> Option<(usize, u32, u32, bool)> {
    without_interrupts(|| {
        let table = TABLE.lock();
        let window = table.get(index)?;
        Some((window.surface, window.width, window.height, window.docked))
    })
}

/// What a window's title bar says.
pub fn title(index: usize) -> alloc::string::String {
    let title = without_interrupts(|| TABLE.lock().get(index).map(|window| window.title));
    title
        .unwrap_or_default()
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as char)
        .collect()
}

/// The title of the first window `owner` opened, if it has one.
pub fn title_of_owner(owner: usize) -> Option<alloc::string::String> {
    let index = without_interrupts(|| TABLE.lock().iter().position(|window| window.owner == owner))?;
    Some(title(index)).filter(|title| !title.is_empty())
}

/// Ask every window titled `title` (8 bytes, as stored) whose program is
/// still running to close. How many were asked.
pub fn close_titled(title: u64) -> usize {
    let title = title.to_le_bytes();
    let matching: Vec<(usize, usize)> = without_interrupts(|| {
        TABLE
            .lock()
            .iter()
            .enumerate()
            .filter(|(_, window)| window.title == title && !window.docked)
            .map(|(index, window)| (index, window.owner))
            .collect()
    });
    let mut asked = 0;
    for (index, owner) in matching {
        if task::state_of(owner) != Some(State::Finished) {
            push(index, window::CLOSE, [0; 4]);
            asked += 1;
        }
    }
    asked
}

/// Whether a window is a popover: no title bar, under the menu bar.
pub fn popover(index: usize) -> bool {
    without_interrupts(|| TABLE.lock().get(index).is_some_and(|window| window.popover))
}

/// Whether a popover is a context menu, placed at the pointer.
pub fn menu(index: usize) -> bool {
    without_interrupts(|| TABLE.lock().get(index).is_some_and(|window| window.menu))
}

/// Whether any window covers the screen: the desktop is locked.
pub fn locked() -> bool {
    let owners: Vec<usize> = without_interrupts(|| {
        TABLE.lock().iter().filter(|window| window.cover).map(|window| window.owner).collect()
    });
    owners.into_iter().any(|owner| task::state_of(owner) != Some(State::Finished))
}

/// Whether a window covers the screen, as the lock screen does.
pub fn cover(index: usize) -> bool {
    without_interrupts(|| TABLE.lock().get(index).is_some_and(|window| window.cover))
}

/// Whether the desktop may resize a window.
pub fn resizable(index: usize) -> bool {
    without_interrupts(|| TABLE.lock().get(index).is_some_and(|window| window.resizable))
}

/// Make a resizable window `width` by `height`, clamped to what windows
/// may be, and tell its program. The size it got, if it could be resized.
pub fn resize(index: usize, width: u32, height: u32) -> Option<(u32, u32)> {
    let (width, height) = (
        width.clamp(64, window::MAX_WIDTH),
        height.clamp(32, window::MAX_HEIGHT),
    );
    // One lock at a time: the window's, then the surface's, then the
    // window's again.
    let surface = without_interrupts(|| {
        let table = TABLE.lock();
        table.get(index).filter(|window| window.resizable).map(|window| window.surface)
    })?;
    if !surface::resize(surface, width, height) {
        return None;
    }
    without_interrupts(|| {
        if let Some(window) = TABLE.lock().get_mut(index) {
            window.width = width;
            window.height = height;
        }
    });
    push(index, window::RESIZE, [width as u64, height as u64, 0, 0]);
    Some((width, height))
}

/// Whether a window's pixels say how transparent they are.
pub fn translucent(index: usize) -> bool {
    without_interrupts(|| TABLE.lock().get(index).is_some_and(|window| window.translucent))
}

/// Whether the program a window was opened for has finished -- exited or
/// been stopped for a fault -- so nobody will draw in it again.
///
/// The surface is not given back: its frames are still mapped in the
/// finished task, and nothing reclaims those yet.
pub fn abandoned(index: usize) -> bool {
    let owner = without_interrupts(|| TABLE.lock().get(index).map(|window| window.owner));
    owner.is_some_and(|owner| task::state_of(owner) == Some(State::Finished))
}

/// Give a window an event, and wake its program if it is waiting.
pub fn push(index: usize, label: u64, words: [u64; 4]) {
    let waiter = without_interrupts(|| {
        let mut table = TABLE.lock();
        let window = table.get_mut(index)?;
        if window.events.len() < QUEUE {
            window.events.push_back((label, words));
        }
        Some(window.waiter)
    });
    if let Some(waiter) = waiter.filter(|waiter| *waiter != NOBODY) {
        task::with_task(waiter, |task| {
            if task.state == State::Blocked {
                task.wake_at = None;
                task.state = State::Ready;
            }
        });
    }
}

/// Map the window in `slot` into the calling task: its pixels, as a surface.
pub fn map(slot: CapPtr) -> Result<surface::Mapped, CapError> {
    let (index, rights) = cap::window_of(slot, Rights::READ)?;
    let (surface, ..) = shape(index).ok_or(CapError::Stale)?;
    surface::map_index(surface, rights)
}

/// Say a rectangle of the window in `slot` has been redrawn. Needs WRITE.
pub fn commit(slot: CapPtr, rect: Rect) -> Result<(), CapError> {
    let (index, _) = cap::window_of(slot, Rights::WRITE)?;
    let (surface, ..) = shape(index).ok_or(CapError::Stale)?;
    surface::commit_index(surface, rect)
}

/// Wait for an event on the window in `slot`, or for the clock to read
/// `deadline`. Needs READ. `None` means the deadline came first.
///
/// One waiter at a time, as for an interrupt.
pub fn wait(slot: CapPtr, deadline: u64) -> Result<Option<(u64, [u64; 4])>, CapError> {
    let (index, _) = cap::window_of(slot, Rights::READ)?;
    let me = task::current_id().ok_or(CapError::NoSuchTask)?;
    loop {
        let taken = without_interrupts(|| {
            let mut table = TABLE.lock();
            let window = table.get_mut(index).ok_or(CapError::Stale)?;
            if let Some(event) = window.events.pop_front() {
                if window.waiter == me {
                    window.waiter = NOBODY;
                }
                return Ok(Some(Some(event)));
            }
            if deadline <= crate::time::now_ns() {
                if window.waiter == me {
                    window.waiter = NOBODY;
                }
                return Ok(Some(None));
            }
            if window.waiter != NOBODY && window.waiter != me {
                return Err(CapError::Refused);
            }
            window.waiter = me;
            // Asleep until an event or the deadline, whichever wakes it.
            // Done under the window's lock, so a `push` cannot land between
            // looking at the queue and going to sleep.
            task::with_task(me, |task| {
                task.wake_at = (deadline != u64::MAX).then_some(deadline);
                task.state = State::Blocked;
            });
            Ok(None)
        })?;
        match taken {
            Some(result) => return Ok(result),
            None => task::yield_now(),
        }
    }
}
