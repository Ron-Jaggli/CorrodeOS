//! Corrode's rendering interface.
//!
//! Shape of the thing, and why it differs from Metal:
//!
//! * **Command buffers are plain data and own no device state.** Recording a
//!   `CommandList` borrows nothing from the renderer, so N threads record N
//!   lists with zero synchronisation and no per-queue serialisation. In Metal a
//!   command buffer is minted from a queue and ordering is tied to creation
//!   order; here ordering is stated explicitly at submit time, which is the one
//!   place it actually matters.
//! * **Commands are POD.** No pointers into host structures, no vtables, no
//!   lifetimes. A `CommandList` can be memcpy'd, sent over IPC to the GPU driver
//!   task, replayed, or lowered to a completely different backend. That is the
//!   "easy to translate" property, and it is the reason a userspace NVIDIA
//!   driver can sit behind the same trait as the software rasteriser.
//! * **No allocation.** Storage is caller-provided, so this works today in a
//!   kernel with no heap and unchanged later in userspace.
//!
//! The software backend is the reference implementation and the thing that must
//! always work. A hardware backend is an optimisation, never a requirement.

#![no_std]

pub mod software;

/// How pixels are packed in a surface's memory.
///
/// Carried as explicit masks and shifts rather than a named-format enum because
/// that is what firmware hands us, and guessing is how you get swapped red and
/// blue channels on someone else's laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u16,
    pub red_mask_size: u8,
    pub red_shift: u8,
    pub green_mask_size: u8,
    pub green_shift: u8,
    pub blue_mask_size: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// The overwhelmingly common case: 32bpp little-endian XRGB.
    pub const XRGB8888: Self = Self {
        bits_per_pixel: 32,
        red_mask_size: 8,
        red_shift: 16,
        green_mask_size: 8,
        green_shift: 8,
        blue_mask_size: 8,
        blue_shift: 0,
    };

    /// Pack a colour into this surface's native pixel representation.
    pub const fn pack(&self, color: Color) -> u32 {
        let r = (color.r as u32) >> (8 - self.red_mask_size);
        let g = (color.g as u32) >> (8 - self.green_mask_size);
        let b = (color.b as u32) >> (8 - self.blue_mask_size);
        (r << self.red_shift) | (g << self.green_shift) | (b << self.blue_shift)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const BLACK: Self = Self::rgb(0, 0, 0);
    pub const WHITE: Self = Self::rgb(255, 255, 255);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Geometry and layout of a render target.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceInfo {
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline. Not necessarily `width * bytes_per_pixel` -- firmware
    /// commonly pads rows, and ignoring that skews the image diagonally.
    pub pitch: u32,
    pub format: PixelFormat,
}

/// A single drawing operation.
///
/// Deliberately POD: `Copy`, no references, no padding-sensitive layout
/// requirements beyond what the compiler gives us. Adding a variant that holds a
/// pointer or a lifetime breaks the IPC and translation story, so don't.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    /// Fill the entire target.
    Clear { color: Color },
    /// Fill an axis-aligned rectangle. Clipped to the target by the backend.
    FillRect { rect: Rect, color: Color },
}

/// A recorded sequence of commands, backed by caller-supplied storage.
///
/// Holds no reference to a renderer, so it is trivially `Send` and can be
/// recorded on any thread.
pub struct CommandList<'a> {
    storage: &'a mut [Command],
    len: usize,
}

impl<'a> CommandList<'a> {
    pub fn new(storage: &'a mut [Command]) -> Self {
        Self { storage, len: 0 }
    }

    /// Append a command. Returns `false` if the backing storage is full --
    /// recording never panics and never allocates, so a full buffer is a normal
    /// condition the caller flushes and retries.
    pub fn push(&mut self, command: Command) -> bool {
        if self.len == self.storage.len() {
            return false;
        }
        self.storage[self.len] = command;
        self.len += 1;
        true
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn commands(&self) -> &[Command] {
        &self.storage[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A backend that can execute recorded command lists.
///
/// Implementors: the software rasteriser today, a userspace NVIDIA driver task
/// later. Nothing in this trait may assume a GPU exists.
pub trait Renderer {
    type Error: core::fmt::Debug;

    fn surface_info(&self) -> SurfaceInfo;

    /// Execute every command in `list`, in order.
    fn submit(&mut self, list: &CommandList<'_>) -> Result<(), Self::Error>;
}
