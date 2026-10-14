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

#![cfg_attr(not(test), no_std)]

pub mod anim;
pub mod blend;
pub mod damage;
pub mod scene;
pub mod software;
pub mod wire;

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

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Edges as `(left, top, right, bottom)`, widened so that `x + width`
    /// cannot overflow.
    const fn edges(&self) -> (i64, i64, i64, i64) {
        let (x, y) = (self.x as i64, self.y as i64);
        (x, y, x + self.width as i64, y + self.height as i64)
    }

    fn from_edges(left: i64, top: i64, right: i64, bottom: i64) -> Self {
        Self {
            x: left as i32,
            y: top as i32,
            width: (right - left).min(u32::MAX as i64) as u32,
            height: (bottom - top).min(u32::MAX as i64) as u32,
        }
    }

    /// The part of this rectangle that is also inside `other`, if any.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let (a, b) = (self.edges(), other.edges());
        let (left, top) = (a.0.max(b.0), a.1.max(b.1));
        let (right, bottom) = (a.2.min(b.2), a.3.min(b.3));
        if left >= right || top >= bottom {
            return None;
        }
        Some(Self::from_edges(left, top, right, bottom))
    }

    /// Whether every pixel of `other` is inside this rectangle.
    pub fn contains(&self, other: &Rect) -> bool {
        !other.is_empty() && self.intersect(other) == Some(*other)
    }

    /// The smallest rectangle containing both. An empty rectangle contributes
    /// nothing, wherever it claims to be.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let (a, b) = (self.edges(), other.edges());
        Self::from_edges(a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
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
    /// Put a colour over whatever is already inside a rectangle, at `alpha`
    /// out of 255. See [`blend`] for the arithmetic.
    ///
    /// The only command that reads the target. On a backend drawing straight
    /// into video memory that read is the expensive part, which is one of the
    /// two reasons a back buffer exists.
    BlendRect { rect: Rect, color: Color, alpha: u8 },
    /// Put a colour through a stencil: a block of the mask atlas, where each
    /// byte says how much of its pixel to cover.
    ///
    /// This is how text is drawn -- the text engine fills in coverage, and
    /// has no idea what colour it will be -- and equally how anything else
    /// with a soft or irregular edge is. `src` is the block of the atlas to
    /// use and `x`, `y` where its top-left corner goes on the target. Each
    /// pixel is blended at `coverage * alpha / 255`.
    ///
    /// The atlas is named by position, not by pointer, so the command stays
    /// plain data. A `src` reaching outside the atlas is clipped to it.
    Mask {
        x: i32,
        y: i32,
        src: Rect,
        color: Color,
        alpha: u8,
    },
    /// Put a block of somebody else's pixels on the target: a window's
    /// contents, at `alpha` out of 255.
    ///
    /// `surface` is a number the backend was given pixels for -- memory it
    /// was shown here, an imported texture on a GPU -- so the command stays
    /// plain data, the same as [`Command::Mask`] naming the atlas by
    /// position. `src` is the block to take and `x`, `y` where its top-left
    /// corner goes. A `src` reaching outside the surface is clipped to it,
    /// and a surface the backend does not have draws nothing.
    ///
    /// Surface pixels are `0x00RRGGBB` whatever the screen's own layout is.
    /// A client draws without knowing what it is being shown on.
    Image {
        x: i32,
        y: i32,
        src: Rect,
        surface: u32,
        alpha: u8,
    },
    /// Confine every later command in this list, `Clear` included, to `rect`.
    ///
    /// This is what makes repainting a damaged region cost that region's
    /// pixels rather than the screen's: replay the whole scene under a scissor
    /// and only the stale part is written. Replaces any earlier scissor, and
    /// does not outlive the list -- each `submit` starts unconfined.
    Scissor { rect: Rect },
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
    ///
    /// Nothing drawn is guaranteed to be on the glass until it is presented.
    fn submit(&mut self, list: &CommandList<'_>) -> Result<(), Self::Error>;

    /// Copy coverage into the mask atlas at `at`: `at.width * at.height`
    /// bytes, row by row.
    ///
    /// The atlas belongs to the backend -- ordinary memory here, a texture on
    /// a GPU -- which is why coverage is handed over rather than pointed at.
    /// Layout of the atlas is the caller's business; the backend only
    /// refuses a block that does not fit.
    fn upload_mask(&mut self, at: Rect, coverage: &[u8]) -> Result<(), Self::Error>;

    /// Make everything drawn inside `damage` visible, and nothing else.
    ///
    /// This is the line between composing a frame and showing it. A viewer
    /// only ever sees the state at a `present`, never a region that has been
    /// cleared and not yet drawn over. Taking the damage rather than flipping
    /// a whole surface keeps the cost of showing a frame proportional to what
    /// changed in it, the same as the cost of drawing it.
    fn present(&mut self, damage: &damage::Damage) -> Result<(), Self::Error>;
}
