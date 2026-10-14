//! Software rasteriser for a linear framebuffer.
//!
//! This is the reference backend. It must work on any machine that can give us
//! a framebuffer, which means no SIMD assumptions, no GPU, no allocation.
//!
//! # Two buffers
//!
//! Given a back buffer, every command draws there and [`Renderer::present`]
//! copies finished regions to the screen. Without one, commands draw straight
//! onto the screen and `present` has nothing to do. The back buffer earns its
//! memory twice:
//!
//! * **Nothing half-drawn is ever visible.** A region is repainted by clearing
//!   it and drawing everything in it back to front. Done on the screen, the
//!   display can scan out the cleared state; done off it, the only thing that
//!   reaches the glass is the finished rows.
//! * **Blending reads.** Firmware maps video memory write-combined, which is
//!   fast to fill and very slow to read back. Blending onto the screen would
//!   put that read on every pixel of every fade. The back buffer is ordinary
//!   cached RAM, and the screen is only ever written.

use crate::blend::Blend;
use crate::damage::Damage;
use crate::{Color, Command, CommandList, Rect, Renderer, SurfaceInfo};

#[derive(Debug)]
pub enum Error {
    /// Backend only handles 32bpp packed formats so far.
    UnsupportedFormat { bits_per_pixel: u16 },
    /// A mask was uploaded with no atlas attached, outside the atlas, or
    /// with the wrong number of bytes for its rectangle.
    BadMask,
}

/// Coverage for [`Command::Mask`] to draw through: one byte per pixel.
struct Atlas {
    data: &'static mut [u8],
    width: u32,
    height: u32,
}

/// A linear framebuffer the CPU draws into directly.
pub struct SoftwareRenderer {
    front: *mut u8,
    /// Where commands draw when it is not the screen. Tightly packed: its
    /// pitch is `width * 4`, whatever padding the screen's rows carry.
    back: Option<*mut u8>,
    atlas: Option<Atlas>,
    info: SurfaceInfo,
}

// SAFETY: `SoftwareRenderer` owns exclusive access to its framebuffer and back
// buffer for its lifetime (the constructors' contracts), and contains no
// thread-affine state. Moving it between threads is therefore sound;
// concurrent *shared* use is still prevented by `submit` requiring `&mut self`.
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
        Ok(Self {
            front: base,
            back: None,
            atlas: None,
            info,
        })
    }

    /// Bytes a back buffer for a surface of this shape must have.
    pub const fn back_buffer_len(info: &SurfaceInfo) -> usize {
        info.width as usize * 4 * info.height as usize
    }

    /// Draw into `back` from now on, and show it only at `present`.
    ///
    /// The buffer's contents are taken as they are. Whoever attaches it is
    /// expected to draw the whole surface before presenting any of it.
    ///
    /// # Safety
    /// * `back` must point at a writable mapping of at least
    ///   [`Self::back_buffer_len`] bytes that does not overlap the
    ///   framebuffer.
    /// * It must stay valid for the lifetime of the renderer, and nothing else
    ///   may read or write it in that time.
    pub unsafe fn attach_back_buffer(&mut self, back: *mut u8) {
        self.back = Some(back);
    }

    pub fn has_back_buffer(&self) -> bool {
        self.back.is_some()
    }

    /// Give the renderer memory to keep masks in: rows of `width` bytes.
    /// Bytes past the last whole row go unused.
    pub fn attach_mask_atlas(&mut self, data: &'static mut [u8], width: u32) {
        let height = (data.len() as u64).checked_div(width as u64).unwrap_or(0);
        self.atlas = Some(Atlas {
            data,
            width,
            height: height.min(u32::MAX as u64) as u32,
        });
    }

    /// The first of `x1 - x0` pixels on row `y` of whatever is being drawn to.
    ///
    /// # Safety
    /// `x0 <= info.width` and `y < info.height`. Callers in this module
    /// guarantee that by clipping first.
    #[inline]
    unsafe fn target_row(&self, x0: u32, y: u32) -> *mut u32 {
        let (base, pitch) = match self.back {
            Some(back) => (back, self.info.width as usize * 4),
            None => (self.front, self.info.pitch as usize),
        };
        // SAFETY: the caller guarantees the position is on the surface, so the
        // offset is within the `pitch * height` bytes the constructor (or
        // `attach_back_buffer`) was promised behind `base`.
        unsafe { base.add(y as usize * pitch + x0 as usize * 4).cast() }
    }

    /// Clip a rectangle to the surface and the scissor, returning
    /// inclusive-exclusive bounds.
    fn clip(&self, rect: Rect, scissor: Option<Rect>) -> Option<(u32, u32, u32, u32)> {
        let rect = match scissor {
            Some(scissor) => rect.intersect(&scissor)?,
            None => rect,
        };
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

    fn fill_rect(&mut self, rect: Rect, color: Color, scissor: Option<Rect>) {
        let Some((x0, y0, x1, y1)) = self.clip(rect, scissor) else {
            return;
        };
        let packed = self.info.format.pack(color);
        let width = (x1 - x0) as usize;
        for y in y0..y1 {
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, y) };
            if row.is_aligned() {
                // A whole row at once, so the compiler emits wide stores
                // rather than one bounds-free write per pixel. Fills are most
                // of what a software desktop does.
                //
                // SAFETY: `row` is aligned, and `width` pixels from it end at
                // x1 <= surface width on a row inside the mapping. `&mut self`
                // makes this the only live reference to those bytes.
                unsafe { core::slice::from_raw_parts_mut(row, width) }.fill(packed);
            } else {
                for x in 0..width {
                    // SAFETY: as above; `write_unaligned` makes no claim
                    // about the pointer beyond its being writable.
                    unsafe { row.add(x).write_unaligned(packed) };
                }
            }
        }
    }

    fn blend_rect(&mut self, rect: Rect, color: Color, alpha: u8, scissor: Option<Rect>) {
        match alpha {
            // The two ends need no arithmetic, and no read of the target.
            0 => return,
            255 => return self.fill_rect(rect, color, scissor),
            _ => {}
        }
        let Some((x0, y0, x1, y1)) = self.clip(rect, scissor) else {
            return;
        };
        let blend = Blend::new(&self.info.format, color, alpha);
        let width = (x1 - x0) as usize;
        for y in y0..y1 {
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, y) };
            for x in 0..width {
                // SAFETY: `x < width` keeps the pixel on this row and inside
                // the surface. Unaligned access so that alignment of a
                // firmware-chosen pitch is not load-bearing.
                unsafe {
                    let pixel = row.add(x);
                    pixel.write_unaligned(blend.over(pixel.read_unaligned()));
                }
            }
        }
    }

    fn mask(
        &mut self,
        (x, y): (i32, i32),
        src: Rect,
        color: Color,
        alpha: u8,
        scissor: Option<Rect>,
    ) {
        let Some(atlas) = &self.atlas else {
            return;
        };
        // The source arrives in a command, which may have arrived from
        // anywhere. Clip it to the atlas first, and move the destination by
        // however much was cut off the top and left.
        let Some(inside) = src.intersect(&Rect::new(0, 0, atlas.width, atlas.height)) else {
            return;
        };
        let (left, top) = (
            x as i64 + (inside.x as i64 - src.x as i64),
            y as i64 + (inside.y as i64 - src.y as i64),
        );
        if left.abs() > i32::MAX as i64 || top.abs() > i32::MAX as i64 {
            return;
        }
        let target = Rect::new(left as i32, top as i32, inside.width, inside.height);
        let Some((x0, y0, x1, y1)) = self.clip(target, scissor) else {
            return;
        };

        let packed = self.info.format.pack(color);
        let width = (x1 - x0) as usize;
        for row_y in y0..y1 {
            // How far into the block this row and its first pixel are.
            let (down, across) = (row_y as i64 - top, x0 as i64 - left);
            let start = (inside.y as i64 + down) as usize * atlas.width as usize
                + (inside.x as i64 + across) as usize;
            let coverage = &atlas.data[start..start + width];

            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, row_y) };
            for (offset, covered) in coverage.iter().enumerate() {
                // Most of a glyph is fully inside or fully outside a stroke,
                // and neither needs the target read.
                match crate::blend::div255(*covered as u32 * alpha as u32) as u8 {
                    0 => {}
                    // SAFETY: `offset < width` keeps the pixel on this row
                    // and inside the surface.
                    255 => unsafe { row.add(offset).write_unaligned(packed) },
                    partial => {
                        let blend = Blend::new(&self.info.format, color, partial);
                        // SAFETY: as above.
                        unsafe {
                            let pixel = row.add(offset);
                            pixel.write_unaligned(blend.over(pixel.read_unaligned()));
                        }
                    }
                }
            }
        }
    }

    fn clear(&mut self, color: Color, scissor: Option<Rect>) {
        self.fill_rect(
            Rect::new(0, 0, self.info.width, self.info.height),
            color,
            scissor,
        );
    }
}

impl Renderer for SoftwareRenderer {
    type Error = Error;

    fn surface_info(&self) -> SurfaceInfo {
        self.info
    }

    fn submit(&mut self, list: &CommandList<'_>) -> Result<(), Self::Error> {
        let mut scissor = None;
        for command in list.commands() {
            match *command {
                Command::Clear { color } => self.clear(color, scissor),
                Command::FillRect { rect, color } => self.fill_rect(rect, color, scissor),
                Command::BlendRect { rect, color, alpha } => {
                    self.blend_rect(rect, color, alpha, scissor)
                }
                Command::Mask {
                    x,
                    y,
                    src,
                    color,
                    alpha,
                } => self.mask((x, y), src, color, alpha, scissor),
                Command::Scissor { rect } => scissor = Some(rect),
            }
        }
        Ok(())
    }

    fn upload_mask(&mut self, at: Rect, coverage: &[u8]) -> Result<(), Self::Error> {
        let atlas = self.atlas.as_mut().ok_or(Error::BadMask)?;
        let fits = Rect::new(0, 0, atlas.width, atlas.height).contains(&at);
        if !fits || coverage.len() as u64 != at.area() {
            return Err(Error::BadMask);
        }
        let width = at.width as usize;
        for (row, bytes) in coverage.chunks_exact(width).enumerate() {
            let start = (at.y as usize + row) * atlas.width as usize + at.x as usize;
            atlas.data[start..start + width].copy_from_slice(bytes);
        }
        Ok(())
    }

    fn present(&mut self, damage: &Damage) -> Result<(), Self::Error> {
        // Without a back buffer everything drawn is on the screen already.
        let Some(back) = self.back else {
            return Ok(());
        };
        let back_pitch = self.info.width as usize * 4;
        let front_pitch = self.info.pitch as usize;

        for rect in damage.rects() {
            let Some((x0, y0, x1, y1)) = self.clip(*rect, None) else {
                continue;
            };
            let (skip, bytes) = (x0 as usize * 4, (x1 - x0) as usize * 4);
            for y in y0 as usize..y1 as usize {
                // SAFETY: `clip` kept the span on the surface, so it lies
                // inside both buffers as their contracts describe them, and
                // `attach_back_buffer` was promised they do not overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        back.add(y * back_pitch + skip),
                        self.front.add(y * front_pitch + skip),
                        bytes,
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;

    const SIDE: u32 = 8;

    /// Render `commands` into an 8x8 surface and return its pixels.
    fn render(commands: &[Command]) -> [u32; (SIDE * SIDE) as usize] {
        let mut pixels = [0u32; (SIDE * SIDE) as usize];
        let info = SurfaceInfo {
            width: SIDE,
            height: SIDE,
            pitch: SIDE * 4,
            format: PixelFormat::XRGB8888,
        };
        // SAFETY: `pixels` is exactly pitch * height writable bytes and
        // outlives the renderer, which is the only thing writing to it.
        let mut renderer =
            unsafe { SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info) }.unwrap();

        let mut storage = [Command::Clear {
            color: Color::BLACK,
        }; 8];
        let mut list = CommandList::new(&mut storage);
        for command in commands {
            assert!(list.push(*command));
        }
        renderer.submit(&list).unwrap();
        pixels
    }

    fn lit(pixels: &[u32]) -> usize {
        pixels.iter().filter(|p| **p != 0).count()
    }

    /// A surface with padded rows and a back buffer behind it.
    struct Buffered {
        front: [u32; Self::PITCH_PIXELS * SIDE as usize],
        back: [u32; (SIDE * SIDE) as usize],
    }

    impl Buffered {
        /// Two pixels of padding per row, as firmware is free to add.
        const PITCH_PIXELS: usize = SIDE as usize + 2;

        fn new() -> Self {
            Self {
                front: [0; Self::PITCH_PIXELS * SIDE as usize],
                back: [0; (SIDE * SIDE) as usize],
            }
        }

        fn renderer(&mut self) -> SoftwareRenderer {
            let info = SurfaceInfo {
                width: SIDE,
                height: SIDE,
                pitch: Self::PITCH_PIXELS as u32 * 4,
                format: PixelFormat::XRGB8888,
            };
            // SAFETY: both arrays are exactly the sizes the contracts ask for,
            // are separate objects, and are only touched through the renderer
            // while it is in use.
            unsafe {
                let mut renderer =
                    SoftwareRenderer::new(self.front.as_mut_ptr().cast(), info).unwrap();
                renderer.attach_back_buffer(self.back.as_mut_ptr().cast());
                renderer
            }
        }

        fn front(&self, x: usize, y: usize) -> u32 {
            self.front[y * Self::PITCH_PIXELS + x]
        }
    }

    fn submit(renderer: &mut SoftwareRenderer, commands: &[Command]) {
        let mut storage = [Command::Clear {
            color: Color::BLACK,
        }; 8];
        let mut list = CommandList::new(&mut storage);
        for command in commands {
            assert!(list.push(*command));
        }
        renderer.submit(&list).unwrap();
    }

    #[test]
    fn nothing_reaches_the_screen_until_it_is_presented() {
        let mut buffers = Buffered::new();
        let mut renderer = buffers.renderer();
        submit(
            &mut renderer,
            &[Command::Clear {
                color: Color::WHITE,
            }],
        );
        assert_eq!(lit(&buffers.front), 0);
        assert_eq!(lit(&buffers.back), (SIDE * SIDE) as usize);

        let mut damage = Damage::new();
        damage.add(Rect::new(0, 0, SIDE, SIDE));
        renderer.present(&damage).unwrap();
        assert_eq!(lit(&buffers.front), (SIDE * SIDE) as usize);
    }

    #[test]
    fn present_shows_the_damage_and_nothing_else() {
        let mut buffers = Buffered::new();
        let mut renderer = buffers.renderer();
        submit(
            &mut renderer,
            &[Command::Clear {
                color: Color::WHITE,
            }],
        );

        let mut damage = Damage::new();
        damage.add(Rect::new(5, 6, 100, 1));
        damage.add(Rect::new(-3, 0, 4, 2));
        renderer.present(&damage).unwrap();

        // Three pixels of row 6, and one column of two rows. Row padding is
        // never written, and the rows land where the screen's pitch puts them.
        assert_eq!(lit(&buffers.front), 5);
        for (x, y) in [(5, 6), (6, 6), (7, 6), (0, 0), (0, 1)] {
            assert_eq!(buffers.front(x, y), 0x00ff_ffff);
        }
        assert_eq!(buffers.front(SIDE as usize, 6), 0);
    }

    #[test]
    fn blend_mixes_with_what_is_underneath() {
        let pixels = render(&[
            Command::Clear {
                color: Color::rgb(0, 100, 255),
            },
            Command::BlendRect {
                rect: Rect::new(2, 2, 2, 2),
                color: Color::rgb(255, 100, 0),
                alpha: 51,
            },
        ]);
        // 51/255 is exactly a fifth.
        assert_eq!(pixels[(2 * SIDE + 2) as usize], 0x0033_64cc);
        assert_eq!(pixels[(2 * SIDE + 1) as usize], 0x0000_64ff);
    }

    #[test]
    fn blend_at_the_ends_is_nothing_or_a_fill() {
        let under = Command::Clear {
            color: Color::rgb(9, 9, 9),
        };
        let over = |alpha| Command::BlendRect {
            rect: Rect::new(0, 0, 8, 8),
            color: Color::WHITE,
            alpha,
        };
        assert!(render(&[under, over(0)]).iter().all(|p| *p == 0x0009_0909));
        assert!(render(&[under, over(255)])
            .iter()
            .all(|p| *p == 0x00ff_ffff));
    }

    /// An 8x8 surface with a 4x4 atlas whose byte at (x, y) is `16 * (4y + x)`
    /// -- except the last, which is 255.
    fn stencilled(commands: &[Command]) -> [u32; (SIDE * SIDE) as usize] {
        let mut pixels = [0u32; (SIDE * SIDE) as usize];
        let info = SurfaceInfo {
            width: SIDE,
            height: SIDE,
            pitch: SIDE * 4,
            format: PixelFormat::XRGB8888,
        };
        // SAFETY: as in `render`.
        let mut renderer =
            unsafe { SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info) }.unwrap();
        renderer.attach_mask_atlas(Box::leak(Box::new([0u8; 16])), 4);

        let mut coverage = [0u8; 16];
        for (index, byte) in coverage.iter_mut().enumerate() {
            *byte = if index == 15 { 255 } else { index as u8 * 16 };
        }
        renderer
            .upload_mask(Rect::new(0, 0, 4, 4), &coverage)
            .unwrap();
        submit(&mut renderer, commands);
        pixels
    }

    #[test]
    fn a_mask_covers_each_pixel_by_its_own_amount() {
        let pixels = stencilled(&[Command::Mask {
            x: 2,
            y: 1,
            src: Rect::new(0, 0, 4, 4),
            color: Color::WHITE,
            alpha: 255,
        }]);
        // Coverage 0 leaves the pixel alone, 255 replaces it, and 16 * n in
        // between is n sixteenths of the way, rounded.
        assert_eq!(pixels[(SIDE + 2) as usize], 0);
        assert_eq!(pixels[(SIDE + 3) as usize], 0x0010_1010);
        assert_eq!(pixels[(2 * SIDE + 2) as usize], 0x0040_4040);
        assert_eq!(pixels[(4 * SIDE + 5) as usize], 0x00ff_ffff);
        assert_eq!(lit(&pixels), 15);
    }

    #[test]
    fn a_mask_is_scaled_by_its_alpha() {
        let pixels = stencilled(&[Command::Mask {
            x: 0,
            y: 0,
            src: Rect::new(2, 3, 2, 1),
            color: Color::rgb(255, 0, 0),
            alpha: 51,
        }]);
        // Atlas (3,3) is 255: a fifth of red. (2,3) is 224: 224 * 51 / 255.
        assert_eq!(pixels[1], 0x0033_0000);
        assert_eq!(pixels[0], 0x002d_0000);
    }

    #[test]
    fn a_mask_reaching_outside_the_atlas_or_the_surface_is_clipped() {
        // Source hangs off the atlas on every side; what is left keeps its
        // place relative to where the whole block was asked to go.
        let pixels = stencilled(&[Command::Mask {
            x: -3,
            y: -3,
            src: Rect::new(-2, -2, 100, 100),
            color: Color::WHITE,
            alpha: 255,
        }]);
        // Atlas (1,1) lands on (0,0); the 3x3 from there is on the surface.
        assert_eq!(pixels[0], 0x0050_5050);
        assert_eq!(pixels[(2 * SIDE + 2) as usize], 0x00ff_ffff);
        assert_eq!(lit(&pixels), 9);

        let scissored = stencilled(&[
            Command::Scissor {
                rect: Rect::new(3, 3, 1, 1),
            },
            Command::Mask {
                x: 0,
                y: 0,
                src: Rect::new(0, 0, 4, 4),
                color: Color::WHITE,
                alpha: 255,
            },
        ]);
        assert_eq!(lit(&scissored), 1);
    }

    #[test]
    fn a_mask_upload_that_does_not_fit_is_refused() {
        let mut pixels = [0u32; 4];
        let info = SurfaceInfo {
            width: 2,
            height: 2,
            pitch: 8,
            format: PixelFormat::XRGB8888,
        };
        // SAFETY: four pixels, exclusively ours.
        let mut renderer =
            unsafe { SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info) }.unwrap();
        let block = Rect::new(0, 0, 2, 2);
        assert!(renderer.upload_mask(block, &[0; 4]).is_err());

        renderer.attach_mask_atlas(Box::leak(Box::new([0u8; 16])), 4);
        assert!(renderer.upload_mask(block, &[0; 4]).is_ok());
        assert!(renderer.upload_mask(block, &[0; 3]).is_err());
        assert!(renderer
            .upload_mask(Rect::new(3, 3, 2, 2), &[0; 4])
            .is_err());
        assert!(renderer
            .upload_mask(Rect::new(-1, 0, 2, 2), &[0; 4])
            .is_err());
    }

    #[test]
    fn blend_respects_the_scissor() {
        let pixels = render(&[
            Command::Scissor {
                rect: Rect::new(0, 0, 1, 1),
            },
            Command::BlendRect {
                rect: Rect::new(-5, -5, 100, 100),
                color: Color::WHITE,
                alpha: 128,
            },
        ]);
        assert_eq!(lit(&pixels), 1);
        assert_eq!(pixels[0], 0x0080_8080);
    }

    #[test]
    fn fill_is_clipped_to_the_surface() {
        let pixels = render(&[Command::FillRect {
            rect: Rect::new(-4, 6, 100, 100),
            color: Color::WHITE,
        }]);
        assert_eq!(lit(&pixels), (SIDE * 2) as usize);
        assert_eq!(pixels[(6 * SIDE) as usize], 0x00ff_ffff);
    }

    #[test]
    fn scissor_confines_fills_and_clears() {
        let pixels = render(&[
            Command::Scissor {
                rect: Rect::new(2, 2, 3, 3),
            },
            Command::Clear {
                color: Color::WHITE,
            },
            Command::FillRect {
                rect: Rect::new(0, 0, 8, 8),
                color: Color::rgb(1, 2, 3),
            },
        ]);
        assert_eq!(lit(&pixels), 9);
        assert_eq!(pixels[(2 * SIDE + 2) as usize], 0x0001_0203);
        assert_eq!(pixels[(2 * SIDE + 1) as usize], 0);
    }

    #[test]
    fn scissor_outside_a_fill_draws_nothing() {
        let pixels = render(&[
            Command::Scissor {
                rect: Rect::new(0, 0, 2, 2),
            },
            Command::FillRect {
                rect: Rect::new(4, 4, 2, 2),
                color: Color::WHITE,
            },
        ]);
        assert_eq!(lit(&pixels), 0);
    }
}
