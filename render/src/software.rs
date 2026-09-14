//! Software rasteriser writing straight into a linear framebuffer.
//!
//! This is the reference backend. It must work on any machine that can give us
//! a framebuffer, which means no SIMD assumptions, no GPU, no allocation.

use crate::{Color, Command, CommandList, Renderer, SurfaceInfo};

#[derive(Debug)]
pub enum Error {
    /// Backend only handles 32bpp packed formats so far.
    UnsupportedFormat { bits_per_pixel: u16 },
}

/// A linear framebuffer the CPU draws into directly.
pub struct SoftwareRenderer {
    base: *mut u8,
    info: SurfaceInfo,
}

// SAFETY: `SoftwareRenderer` owns exclusive access to its framebuffer region for
// its lifetime (the constructor's contract), and contains no thread-affine
// state. Moving it between threads is therefore sound; concurrent *shared* use
// is still prevented by `submit` requiring `&mut self`.
unsafe impl Send for SoftwareRenderer {}

impl SoftwareRenderer {
    /// Wrap a framebuffer supplied by firmware.
    ///
    /// # Safety
    /// * `base` must point at a writable mapping of at least
    ///   `info.pitch * info.height` bytes.
    /// * That region must stay valid and mapped for the lifetime of the
    ///   returned renderer.
    /// * No other code may write to the region concurrently -- this type
    ///   assumes exclusive ownership of those pixels.
    pub unsafe fn new(base: *mut u8, info: SurfaceInfo) -> Result<Self, Error> {
        if info.format.bits_per_pixel != 32 {
            return Err(Error::UnsupportedFormat {
                bits_per_pixel: info.format.bits_per_pixel,
            });
        }
        Ok(Self { base, info })
    }

    /// Write one pixel, no bounds check.
    ///
    /// # Safety
    /// `x < info.width` and `y < info.height` must both hold. Callers in this
    /// module guarantee that by clipping first.
    #[inline]
    unsafe fn put_unchecked(&mut self, x: u32, y: u32, packed: u32) {
        let offset = (y as usize) * (self.info.pitch as usize) + (x as usize) * 4;
        // SAFETY: the caller guarantees x/y are in bounds, so `offset` is within
        // `pitch * height` bytes of `base`, which the constructor's contract
        // says is a writable mapping we exclusively own. The target is 4-byte
        // aligned because pitch and the 4-byte stride are both multiples of 4
        // for any 32bpp firmware framebuffer; `write_unaligned` is used anyway
        // so that assumption is not load-bearing.
        unsafe {
            self.base.add(offset).cast::<u32>().write_unaligned(packed);
        }
    }

    /// Clip a rectangle to the surface, returning inclusive-exclusive bounds.
    fn clip(&self, rect: crate::Rect) -> Option<(u32, u32, u32, u32)> {
        let (sw, sh) = (self.info.width as i64, self.info.height as i64);

        let x0 = rect.x as i64;
        let y0 = rect.y as i64;
        let x1 = x0 + rect.width as i64;
        let y1 = y0 + rect.height as i64;

        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = x1.min(sw);
        let y1 = y1.min(sh);

        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
    }

    fn fill_rect(&mut self, rect: crate::Rect, color: Color) {
        let Some((x0, y0, x1, y1)) = self.clip(rect) else {
            return;
        };
        let packed = self.info.format.pack(color);
        for y in y0..y1 {
            for x in x0..x1 {
                // SAFETY: `clip` returned bounds already intersected with the
                // surface, so x < width and y < height.
                unsafe { self.put_unchecked(x, y, packed) };
            }
        }
    }

    fn clear(&mut self, color: Color) {
        self.fill_rect(
            crate::Rect::new(0, 0, self.info.width, self.info.height),
            color,
        );
    }
}

impl Renderer for SoftwareRenderer {
    type Error = Error;

    fn surface_info(&self) -> SurfaceInfo {
        self.info
    }

    fn submit(&mut self, list: &CommandList<'_>) -> Result<(), Self::Error> {
        for command in list.commands() {
            match *command {
                Command::Clear { color } => self.clear(color),
                Command::FillRect { rect, color } => self.fill_rect(rect, color),
            }
        }
        Ok(())
    }
}
