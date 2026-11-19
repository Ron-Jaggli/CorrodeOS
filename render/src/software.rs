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
//!
//! # Pixels that are not ours
//!
//! A window's contents live in memory its client draws into, and the renderer
//! is shown that memory rather than given a copy: [`Command::Image`] reads it
//! in place. The client can be writing while we read. That is a torn picture
//! at worst -- every bit pattern is a pixel -- and never a bad pointer,
//! because the address and the size came from whoever attached the surface
//! and not from the client. Nothing here ever forms a reference to those
//! bytes.

use crate::blend::Blend;
use crate::damage::Damage;
use crate::glass::{self, Glass, Material, Q8};
use crate::{Color, Command, CommandList, PixelFormat, Rect, Renderer, SurfaceInfo};

#[derive(Debug)]
pub enum Error {
    /// Backend only handles 32bpp packed formats so far.
    UnsupportedFormat { bits_per_pixel: u16 },
    /// A mask was uploaded with no atlas attached, outside the atlas, or
    /// with the wrong number of bytes for its rectangle.
    BadMask,
    /// A surface was attached under a number past [`SURFACES`], or with no
    /// pixels in it.
    BadSurface,
}

/// How many surfaces can be attached at once.
pub const SURFACES: usize = 16;

/// A client's pixels for [`Command::Image`] to draw from: rows of `width`
/// pixels, each `0x00RRGGBB`.
#[derive(Clone, Copy)]
struct Pixels {
    base: *const u32,
    width: u32,
    height: u32,
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
    surfaces: [Option<Pixels>; SURFACES],
    /// Where [`Command::Glass`] copies and blurs its backdrop.
    scratch: Option<&'static mut [u32]>,
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
            surfaces: [None; SURFACES],
            scratch: None,
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

    /// Give the renderer memory for glass to work in. A pane needs three
    /// times the pixels of its rectangle grown by its reach, and a line more;
    /// one that does not fit, or any pane with none attached, is drawn as a
    /// plain veil instead.
    pub fn attach_glass_scratch(&mut self, data: &'static mut [u32]) {
        self.scratch = Some(data);
    }

    /// Show the renderer a block of pixels to draw [`Command::Image`] from,
    /// under the number `surface`. Replaces whatever had that number.
    ///
    /// # Safety
    /// * `base` must be aligned for `u32` and point at a readable mapping of
    ///   at least `width * height * 4` bytes that overlaps neither the
    ///   framebuffer nor the back buffer.
    /// * It must stay mapped until the surface is detached or the renderer
    ///   is gone.
    ///
    /// Nothing is required of the contents. They may be changing as they are
    /// read.
    pub unsafe fn attach_surface(
        &mut self,
        surface: u32,
        base: *const u32,
        width: u32,
        height: u32,
    ) -> Result<(), Error> {
        let slot = self
            .surfaces
            .get_mut(surface as usize)
            .ok_or(Error::BadSurface)?;
        if width == 0 || height == 0 {
            return Err(Error::BadSurface);
        }
        *slot = Some(Pixels {
            base,
            width,
            height,
        });
        Ok(())
    }

    /// Forget a surface. Commands that name it draw nothing from then on.
    pub fn detach_surface(&mut self, surface: u32) {
        if let Some(slot) = self.surfaces.get_mut(surface as usize) {
            *slot = None;
        }
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

    /// [`Command::ScaledMask`]: each pixel of `rect` reads the block between
    /// its four nearest atlas pixels, so the stencil stays smooth however it
    /// is stretched.
    fn scaled_mask(&mut self, rect: Rect, src: Rect, color: Color, alpha: u8, scissor: Option<Rect>) {
        let Some(atlas) = &self.atlas else {
            return;
        };
        let Some(inside) = src.intersect(&Rect::new(0, 0, atlas.width, atlas.height)) else {
            return;
        };
        if rect.is_empty() {
            return;
        }
        let Some((x0, y0, x1, y1)) = self.clip(rect, scissor) else {
            return;
        };
        // Where a target pixel's centre falls in the block, in 256ths of an
        // atlas pixel, less a half so that 0 is the first pixel's centre;
        // and the two pixels either side of it, and how far between.
        let axis = |offset: i64, target: u32, source: u32| {
            let at = ((2 * offset + 1) * source as i64 * 128 / target as i64 - 128).max(0);
            let near = ((at >> 8) as u32).min(source - 1);
            (near, (near + 1).min(source - 1), (at & 255) as u32)
        };
        let packed = self.info.format.pack(color);
        let stride = atlas.width as usize;
        for row_y in y0..y1 {
            let (top, bottom, fy) = axis(row_y as i64 - rect.y as i64, rect.height, inside.height);
            let (top, bottom) = (
                (inside.y as u32 + top) as usize * stride,
                (inside.y as u32 + bottom) as usize * stride,
            );
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, row_y) };
            for x in x0..x1 {
                let (left, right, fx) = axis(x as i64 - rect.x as i64, rect.width, inside.width);
                let (left, right) = ((inside.x as u32 + left) as usize, (inside.x as u32 + right) as usize);
                let [a, b, c, d] = [top + left, top + right, bottom + left, bottom + right]
                    .map(|at| atlas.data[at] as u32);
                let upper = a * (256 - fx) + b * fx;
                let lower = c * (256 - fx) + d * fx;
                let covered = (upper * (256 - fy) + lower * fy) >> 16;
                let pixel_alpha = crate::blend::div255(covered * alpha as u32) as u8;
                // SAFETY: `x` is inside the clipped bounds, on this row.
                let pixel = unsafe { row.add((x - x0) as usize) };
                match pixel_alpha {
                    0 => {}
                    // SAFETY: as above.
                    255 => unsafe { pixel.write_unaligned(packed) },
                    partial => {
                        let blend = Blend::new(&self.info.format, color, partial);
                        // SAFETY: as above.
                        unsafe { pixel.write_unaligned(blend.over(pixel.read_unaligned())) };
                    }
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

    fn image(
        &mut self,
        (x, y): (i32, i32),
        src: Rect,
        surface: u32,
        alpha: u8,
        translucent: bool,
        radius: u8,
        scissor: Option<Rect>,
    ) {
        if alpha == 0 {
            return;
        }
        let Some(Some(pixels)) = self.surfaces.get(surface as usize).copied() else {
            return;
        };
        // As for a mask: clip the source to what there is of it, and move
        // the destination by whatever came off the top and left.
        let Some(inside) = src.intersect(&Rect::new(0, 0, pixels.width, pixels.height)) else {
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

        let format = self.info.format;
        // A surface's layout is the commonest screen layout, on purpose: an
        // opaque window is then copied a row at a time and never looked at.
        let same_layout = format == crate::PixelFormat::XRGB8888;
        let width = (x1 - x0) as usize;
        for row_y in y0..y1 {
            let (down, across) = (row_y as i64 - top, x0 as i64 - left);
            let start = (inside.y as i64 + down) as usize * pixels.width as usize
                + (inside.x as i64 + across) as usize;
            // SAFETY: `inside` is within the surface and the clip only shrank
            // it, so `start` and the `width` pixels after it are among the
            // `width * height` that `attach_surface` was promised.
            let source = unsafe { pixels.base.add(start) };
            // SAFETY: `clip` returned bounds already intersected with the
            // target.
            let row = unsafe { self.target_row(x0, row_y) };
            // Rows the lower corners cut into, measured on the image as a
            // whole, wherever the clip has put this row.
            let (full_w, full_h) = (inside.width as i64 * Q8, inside.height as i64 * Q8);
            let corner_row = radius > 0 && row_y as i64 >= top + inside.height as i64 - radius as i64;
            let ly = (row_y as i64 - top) * Q8 + Q8 / 2;

            if alpha == 255 && same_layout && !translucent && !corner_row {
                // SAFETY: both spans are `width` pixels inside their buffers,
                // and `attach_surface` was promised they do not overlap.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        source.cast::<u8>(),
                        row.cast::<u8>(),
                        width * 4,
                    );
                }
                continue;
            }
            for offset in 0..width {
                // SAFETY: `offset < width` keeps both pixels inside the spans
                // described above. Read by value: the client may be writing.
                unsafe {
                    let pixel = source.add(offset).read_volatile();
                    let color = Color::rgb((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
                    // An opaque surface's top byte is whatever the client
                    // left there, and means nothing.
                    let mut alpha = match translucent {
                        true => crate::blend::div255((255 - (pixel >> 24)) * alpha as u32) as u8,
                        false => alpha,
                    };
                    if corner_row {
                        let lx = (x0 as i64 + offset as i64 - left) * Q8 + Q8 / 2;
                        let field = glass::field(lx, ly, full_w, full_h, radius as i64 * Q8);
                        alpha = crate::blend::div255(glass::coverage(&field) * alpha as u32) as u8;
                    }
                    let target = row.add(offset);
                    match alpha {
                        0 => {}
                        255 => target.write_unaligned(format.pack(color)),
                        _ => target.write_unaligned(
                            Blend::new(&format, color, alpha).over(target.read_unaligned()),
                        ),
                    }
                }
            }
        }
    }

    /// The soft shadow around a rounded rectangle; see [`Command::Shadow`].
    fn shadow(&mut self, rect: Rect, radius: u16, blur: u8, alpha: u8, scissor: Option<Rect>) {
        if alpha == 0 || rect.is_empty() {
            return;
        }
        let blur = blur.max(1) as i32;
        let reach = Rect::new(
            rect.x.saturating_sub(blur),
            rect.y.saturating_sub(blur),
            rect.width.saturating_add(2 * blur as u32),
            rect.height.saturating_add(2 * blur as u32),
        );
        let Some((x0, y0, x1, y1)) = self.clip(reach, scissor) else {
            return;
        };
        let format = self.info.format;
        let (width, height) = (rect.width as i64 * Q8, rect.height as i64 * Q8);
        let radius = radius as i64 * Q8;
        let spread = blur as i64 * Q8;
        for y in y0..y1 {
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, y) };
            let ly = (y as i64 - rect.y as i64) * Q8 + Q8 / 2;
            for x in x0..x1 {
                let lx = (x as i64 - rect.x as i64) * Q8 + Q8 / 2;
                let distance = glass::field(lx, ly, width, height, radius).distance;
                if distance <= 0 || distance >= spread {
                    continue;
                }
                // Smoothstep from the edge out: a Gaussian's look, for the
                // price of a square.
                let t = 4096 - distance * 4096 / spread;
                let fall = t * t / 4096 * (3 * 4096 - 2 * t) / 4096;
                let dark = (fall * alpha as i64 / 4096) as u8;
                if dark == 0 {
                    continue;
                }
                // SAFETY: `x` is inside the clipped bounds, on this row.
                unsafe {
                    let pixel = row.add((x - x0) as usize);
                    pixel.write_unaligned(
                        Blend::new(&format, Color::BLACK, dark).over(pixel.read_unaligned()),
                    );
                }
            }
        }
    }

    /// A pane of glass over what is already in `rect`. See [`glass`].
    fn glass(&mut self, rect: Rect, pane: &Glass, alpha: u8, scissor: Option<Rect>) {
        if alpha == 0 || rect.is_empty() {
            return;
        }
        let Some((x0, y0, x1, y1)) = self.clip(rect, scissor) else {
            return;
        };
        // What it reads: as far around the pane as the blur reaches,
        // whatever the scissor, since that much was promised painted.
        let reach = pane.reach().min(4096) as i32;
        let around = Rect::new(
            rect.x.saturating_sub(reach),
            rect.y.saturating_sub(reach),
            rect.width.saturating_add(2 * reach as u32),
            rect.height.saturating_add(2 * reach as u32),
        );
        let Some((ax0, ay0, ax1, ay1)) = self.clip(around, None) else {
            return;
        };
        // Of that, only what the pixels being drawn can see: as far around
        // them as the blur reaches and the lens bends (half the refraction
        // at most), and a little for rounding. A pane drawn in pieces, one
        // per damaged rectangle, is then blurred once over, not once per
        // piece. Every pixel drawn is still a whole reach from the edge, so
        // it comes out as it would with the whole backdrop, and the start
        // keeps the parity of the whole one's, so the half-size grid below
        // lines up from piece to piece and there are no seams.
        let margin = reach as i64 + pane.refraction as i64 / 2 + 2;
        let start = |whole: u32, drawn: u32| {
            let from = (drawn as i64 - margin).max(whole as i64);
            whole + ((from - whole as i64) as u32 & !1)
        };
        let (bx0, by0) = (start(ax0, x0), start(ay0, y0));
        let bx1 = ((x1 as i64 + margin) as u32).min(ax1);
        let by1 = ((y1 as i64 + margin) as u32).min(ay1);
        let (bw, bh) = ((bx1 - bx0) as usize, (by1 - by0) as usize);
        let format = self.info.format;
        // Frosted glass is blurred at half size and read back smoothly: a
        // blur this wide leaves no detail a full-size one would keep, and
        // it is a quarter of the work. Clear glass is not blurred, so is
        // read at full size, where its lens can bend every pixel.
        let (frost_radius, fill_radius) = pane.blur_radii();
        let frosted = frost_radius + fill_radius > 0;
        let scale = if frosted { 2 } else { 1 };
        let (sw, sh) = (bw.div_ceil(scale), bh.div_ceil(scale));
        let area = sw * sh;
        let needed = if frosted { 2 * area } else { area } + sw.max(sh);

        let shape = (
            rect.width as i64 * Q8,
            rect.height as i64 * Q8,
            pane.radius as i64 * Q8,
        );
        let mut scratch = self.scratch.take();
        let Some(buffers) = scratch.as_deref_mut().filter(|data| data.len() >= needed) else {
            self.scratch = scratch;
            return self.veil(rect, shape, pane, alpha, (x0, y0, x1, y1));
        };
        let (backdrop, rest) = buffers.split_at_mut(area);

        if frosted {
            // Each pixel of the small backdrop is the mean of the (up to)
            // four it stands for.
            for sy in 0..sh {
                for sx in 0..sw {
                    let mut sum = [0u32; 3];
                    let mut count = 0;
                    for y in sy * 2..(sy * 2 + 2).min(bh) {
                        // SAFETY: `clip` kept the backdrop on the surface.
                        let row = unsafe { self.target_row(bx0, by0 + y as u32) };
                        for x in sx * 2..(sx * 2 + 2).min(bw) {
                            // SAFETY: `x < bw` keeps the pixel on this row.
                            let pixel = unpack(&format, unsafe { row.add(x).read_unaligned() });
                            for (total, channel) in sum.iter_mut().zip(glass::channels(pixel)) {
                                *total += channel;
                            }
                            count += 1;
                        }
                    }
                    backdrop[sy * sw + sx] = glass::pack(sum.map(|total| total / count));
                }
            }
        } else {
            for y in 0..bh {
                // SAFETY: `clip` kept the backdrop on the surface.
                let row = unsafe { self.target_row(bx0, by0 + y as u32) };
                for x in 0..bw {
                    // SAFETY: `x < bw` keeps the pixel on this row.
                    let pixel = unsafe { row.add(x).read_unaligned() };
                    backdrop[y * bw + x] = unpack(&format, pixel);
                }
            }
        }

        // Frost: three boxes for each Gaussian, then the veil between them,
        // with each radius halved for the half-size backdrop.
        if frosted {
            let (fill, rest) = rest.split_at_mut(area);
            let line = &mut rest[..sw.max(sh)];
            fill.copy_from_slice(backdrop);
            for _ in 0..3 {
                glass::box_blur(backdrop, sw, sh, frost_radius as usize / scale, line);
                glass::box_blur(fill, sw, sh, fill_radius as usize / scale, line);
            }
            let veil = pane.fill_opacity();
            for (frosted, filled) in backdrop.iter_mut().zip(fill.iter()) {
                let (a, b) = (glass::channels(*frosted), glass::channels(*filled));
                *frosted = glass::pack([0, 1, 2].map(|i| {
                    let blend = if pane.dark { a[i].min(b[i]) } else { a[i].max(b[i]) };
                    (blend * veil + a[i] * (255 - veil)) / 255
                }));
            }
        }
        let backdrop: &[u32] = backdrop;
        // The backdrop at full-size pixel `x`, `y` (from its corner): read
        // straight, or between the four small pixels around it.
        let seen_at = |x: usize, y: usize| -> u32 {
            if scale == 1 {
                return backdrop[y * bw + x];
            }
            // A full pixel's centre, in small pixels, in 256ths: (2x + 1) / 4
            // less a half.
            let axis = |at: usize, size: usize| {
                let q = (at as i64 * 128 - 64).max(0);
                let i0 = ((q >> 8) as usize).min(size - 1);
                (i0, (i0 + 1).min(size - 1), (q & 255) as u32)
            };
            let (x0, x1, fx) = axis(x, sw);
            let (y0, y1, fy) = axis(y, sh);
            let [a, b, c, d] = [
                backdrop[y0 * sw + x0],
                backdrop[y0 * sw + x1],
                backdrop[y1 * sw + x0],
                backdrop[y1 * sw + x1],
            ]
            .map(glass::channels);
            glass::pack([0, 1, 2].map(|i| {
                let top = a[i] * (256 - fx) + b[i] * fx;
                let bottom = c[i] * (256 - fx) + d[i] * fx;
                (top * (256 - fy) + bottom * fy) >> 16
            }))
        };

        let tone = pane.tone();
        let chroma = pane.chroma();
        let (width, height, radius) = shape;
        for y in y0..y1 {
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, y) };
            let ly = (y as i64 - rect.y as i64) * Q8 + Q8 / 2;
            for x in x0..x1 {
                let lx = (x as i64 - rect.x as i64) * Q8 + Q8 / 2;
                let field = glass::field(lx, ly, width, height, radius);
                let covered = glass::coverage(&field);
                if covered == 0 {
                    continue;
                }
                let lens = field.distance > -glass::BEZEL * Q8;
                let (dx, dy) = match lens && pane.refraction > 0 {
                    true => glass::displacement(&field, pane.refraction),
                    false => (0, 0),
                };
                let sx = (x as i64 + (dx + Q8 / 2).div_euclid(Q8)).clamp(bx0 as i64, bx1 as i64 - 1);
                let sy = (y as i64 + (dy + Q8 / 2).div_euclid(Q8)).clamp(by0 as i64, by1 as i64 - 1);
                let seen = seen_at(sx as usize - bx0 as usize, sy as usize - by0 as usize);

                let mut rgb = glass::channels(seen).map(|c| tone[c as usize] as u32);
                rgb = glass::saturate(rgb, chroma);
                rgb = glass::tint(rgb, pane);
                if pane.rim && field.distance > -5 * Q8 {
                    let (light, opacity) = glass::rim(&field, pane.dark);
                    rgb = rgb.map(|c| (c * (255 - opacity) / 255 + light).min(255));
                }
                let color = Color::rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8);
                let alpha = crate::blend::div255(covered * alpha as u32) as u8;
                // SAFETY: `x` is inside the clipped bounds, on this row.
                unsafe {
                    let pixel = row.add((x - x0) as usize);
                    pixel.write_unaligned(match alpha {
                        255 => format.pack(color),
                        _ => Blend::new(&format, color, alpha).over(pixel.read_unaligned()),
                    });
                }
            }
        }
        self.scratch = scratch;
    }

    /// Glass with nowhere to work: the material's colour, laid over the
    /// pane's shape with no blur and no lens.
    fn veil(
        &mut self,
        rect: Rect,
        (width, height, radius): (i64, i64, i64),
        pane: &Glass,
        alpha: u8,
        (x0, y0, x1, y1): (u32, u32, u32, u32),
    ) {
        let (color, opacity) = match (pane.material, pane.dark) {
            (Material::Regular, false) => (Color::rgb(0xf4, 0xf4, 0xf6), 200),
            (Material::Regular, true) => (Color::rgb(0x26, 0x27, 0x2c), 210),
            (Material::Clear, false) => (Color::rgb(0xff, 0xff, 0xff), 60),
            (Material::Clear, true) => (Color::rgb(0x30, 0x30, 0x34), 90),
        };
        let format = self.info.format;
        for y in y0..y1 {
            // SAFETY: `clip` returned bounds already intersected with the
            // surface.
            let row = unsafe { self.target_row(x0, y) };
            let ly = (y as i64 - rect.y as i64) * Q8 + Q8 / 2;
            for x in x0..x1 {
                let lx = (x as i64 - rect.x as i64) * Q8 + Q8 / 2;
                let covered = glass::coverage(&glass::field(lx, ly, width, height, radius));
                let alpha = crate::blend::div255(covered * opacity * alpha as u32 / 255) as u8;
                if alpha == 0 {
                    continue;
                }
                // SAFETY: `x` is inside the clipped bounds, on this row.
                unsafe {
                    let pixel = row.add((x - x0) as usize);
                    pixel.write_unaligned(Blend::new(&format, color, alpha).over(pixel.read_unaligned()));
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

/// A pixel in the target's layout as `0x00RRGGBB`.
fn unpack(format: &PixelFormat, pixel: u32) -> u32 {
    if *format == PixelFormat::XRGB8888 {
        return pixel & 0x00ff_ffff;
    }
    let channel = |shift: u8, size: u8| {
        let size = size.clamp(1, 8);
        let value = (pixel >> shift) & ((1 << size) - 1);
        if size == 8 {
            return value;
        }
        // Widened to eight bits with the top bits repeated, so full is 255.
        (value << (8 - size)) | (value >> (size - (8 - size).min(size)))
    };
    channel(format.red_shift, format.red_mask_size) << 16
        | channel(format.green_shift, format.green_mask_size) << 8
        | channel(format.blue_shift, format.blue_mask_size)
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
                Command::ScaledMask {
                    rect,
                    src,
                    color,
                    alpha,
                } => self.scaled_mask(rect, src, color, alpha, scissor),
                Command::Image {
                    x,
                    y,
                    src,
                    surface,
                    alpha,
                    translucent,
                    radius,
                } => self.image((x, y), src, surface, alpha, translucent, radius, scissor),
                Command::Shadow {
                    rect,
                    radius,
                    blur,
                    alpha,
                } => self.shadow(rect, radius, blur, alpha, scissor),
                Command::Scissor { rect } => scissor = Some(rect),
                Command::Glass { rect, glass, alpha } => self.glass(rect, &glass, alpha, scissor),
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
    fn a_scaled_mask_at_its_own_size_is_a_mask() {
        let src = Rect::new(0, 0, 4, 4);
        let plain = stencilled(&[Command::Mask {
            x: 2,
            y: 1,
            src,
            color: Color::WHITE,
            alpha: 255,
        }]);
        let scaled = stencilled(&[Command::ScaledMask {
            rect: Rect::new(2, 1, 4, 4),
            src,
            color: Color::WHITE,
            alpha: 255,
        }]);
        assert_eq!(plain, scaled);
    }

    #[test]
    fn a_scaled_mask_fills_its_rect_and_stays_smooth() {
        // The atlas's last pixel, 255, over the whole surface.
        let full = stencilled(&[Command::ScaledMask {
            rect: Rect::new(0, 0, SIDE, SIDE),
            src: Rect::new(3, 3, 1, 1),
            color: Color::WHITE,
            alpha: 255,
        }]);
        assert!(full.iter().all(|pixel| *pixel == 0x00ff_ffff));

        // The first row, 0 16 32 48, twice as wide: never darker to the
        // right, and its ends where the row's are.
        let wide = stencilled(&[Command::ScaledMask {
            rect: Rect::new(0, 0, SIDE, 1),
            src: Rect::new(0, 0, 4, 1),
            color: Color::WHITE,
            alpha: 255,
        }]);
        let row: std::vec::Vec<u32> = wide[..SIDE as usize].iter().map(|pixel| pixel & 0xff).collect();
        assert!(row.windows(2).all(|pair| pair[0] <= pair[1]), "{row:?}");
        assert_eq!((row[0], row[SIDE as usize - 1]), (0, 48));
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

    /// Side of the test surface, which is smaller than the screen.
    const PICTURE: u32 = 4;

    /// A surface where every pixel says where it is.
    fn picture() -> [u32; (PICTURE * PICTURE) as usize] {
        core::array::from_fn(|i| 0x0010_2000 + i as u32)
    }

    /// Render `commands` over grey with [`picture`] attached as surface 3.
    fn pictured(commands: &[Command]) -> [u32; (SIDE * SIDE) as usize] {
        let picture = picture();
        let mut pixels = [0u32; (SIDE * SIDE) as usize];
        let info = SurfaceInfo {
            width: SIDE,
            height: SIDE,
            pitch: SIDE * 4,
            format: PixelFormat::XRGB8888,
        };
        // SAFETY: `pixels` is exactly pitch * height writable bytes, and
        // `picture` is PICTURE * PICTURE pixels somewhere else. Both outlive
        // the renderer.
        let mut renderer = unsafe {
            let mut renderer = SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info).unwrap();
            renderer
                .attach_surface(3, picture.as_ptr(), PICTURE, PICTURE)
                .unwrap();
            renderer
        };
        submit(
            &mut renderer,
            &[Command::Clear {
                color: Color::rgb(100, 100, 100),
            }],
        );
        submit(&mut renderer, commands);
        pixels
    }

    const GREY: u32 = 0x0064_6464;

    fn whole_picture(x: i32, y: i32, alpha: u8) -> Command {
        Command::Image {
            x,
            y,
            src: Rect::new(0, 0, PICTURE, PICTURE),
            surface: 3,
            alpha,
            translucent: false,
            radius: 0,
        }
    }

    #[test]
    fn an_image_puts_each_of_its_pixels_where_it_belongs() {
        let pixels = pictured(&[whole_picture(2, 3, 255)]);
        let picture = picture();
        for y in 0..SIDE {
            for x in 0..SIDE {
                let inside = (2..2 + PICTURE).contains(&x) && (3..3 + PICTURE).contains(&y);
                let expected = if inside {
                    picture[((y - 3) * PICTURE + (x - 2)) as usize]
                } else {
                    GREY
                };
                assert_eq!(pixels[(y * SIDE + x) as usize], expected, "at {x}, {y}");
            }
        }
    }

    #[test]
    fn an_image_is_blended_at_its_alpha() {
        let pixels = pictured(&[whole_picture(0, 0, 77)]);
        let picture = picture();
        for (i, source) in picture.iter().enumerate() {
            let (x, y) = (i as u32 % PICTURE, i as u32 / PICTURE);
            let channel = |shift: u32| crate::blend::mix((source >> shift) as u8, 100, 77) as u32;
            assert_eq!(
                pixels[(y * SIDE + x) as usize],
                (channel(16) << 16) | (channel(8) << 8) | channel(0)
            );
        }
        assert_eq!(pictured(&[whole_picture(0, 0, 0)]), [GREY; 64]);
    }

    #[test]
    fn an_image_reaching_outside_its_surface_or_the_screen_is_clipped() {
        let picture = picture();

        // A source hanging off every side of the surface: what is drawn is
        // the part that exists, and it lands where that part belongs.
        let pixels = pictured(&[Command::Image {
            x: 0,
            y: 0,
            src: Rect::new(-2, -1, 40, 40),
            surface: 3,
            alpha: 255,
            translucent: false,
            radius: 0,
        }]);
        assert_eq!(pixels[0], GREY);
        assert_eq!(pixels[(SIDE + 2) as usize], picture[0]);
        assert_eq!(pixels[(4 * SIDE + 5) as usize], picture[15]);
        assert_eq!(pixels.iter().filter(|p| **p != GREY).count(), 16);

        // Off the top-left of the screen: only the bottom-right pixel shows.
        let pixels = pictured(&[whole_picture(-3, -3, 255)]);
        assert_eq!(pixels[0], picture[15]);
        assert_eq!(pixels.iter().filter(|p| **p != GREY).count(), 1);

        // Under a scissor, and half blended so the other path is clipped too.
        for alpha in [255, 128] {
            let pixels = pictured(&[
                Command::Scissor {
                    rect: Rect::new(1, 1, 2, 1),
                },
                whole_picture(0, 0, alpha),
            ]);
            assert_eq!(pixels.iter().filter(|p| **p != GREY).count(), 2);
            assert_ne!(pixels[(SIDE + 1) as usize], GREY);
            assert_ne!(pixels[(SIDE + 2) as usize], GREY);
        }
    }

    #[test]
    fn an_image_of_a_surface_that_is_not_there_draws_nothing() {
        let mut command = whole_picture(0, 0, 255);
        for missing in [0, 7, 8, u32::MAX] {
            if let Command::Image { surface, .. } = &mut command {
                *surface = missing;
            }
            assert_eq!(pictured(&[command]), [GREY; 64]);
        }
    }

    #[test]
    fn a_surface_can_be_refused_replaced_and_detached() {
        let (first, second) = ([0x11u32; 4], [0x22u32; 4]);
        let mut pixels = [0u32; (SIDE * SIDE) as usize];
        let info = SurfaceInfo {
            width: SIDE,
            height: SIDE,
            pitch: SIDE * 4,
            format: PixelFormat::XRGB8888,
        };
        let image = Command::Image {
            x: 0,
            y: 0,
            src: Rect::new(0, 0, 2, 2),
            surface: 0,
            alpha: 255,
            translucent: false,
            radius: 0,
        };
        // SAFETY: as in `pictured`; each surface is 2 * 2 pixels of its own.
        unsafe {
            let mut renderer = SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info).unwrap();
            assert!(renderer
                .attach_surface(SURFACES as u32, first.as_ptr(), 2, 2)
                .is_err());
            assert!(renderer.attach_surface(0, first.as_ptr(), 0, 2).is_err());

            renderer.attach_surface(0, first.as_ptr(), 2, 2).unwrap();
            submit(&mut renderer, &[image]);
            assert_eq!(pixels[0], 0x11);

            renderer.attach_surface(0, second.as_ptr(), 2, 2).unwrap();
            submit(&mut renderer, &[image]);
            assert_eq!(pixels[0], 0x22);

            renderer.detach_surface(0);
            submit(
                &mut renderer,
                &[
                    Command::Clear {
                        color: Color::BLACK,
                    },
                    image,
                ],
            );
            assert_eq!(pixels[0], 0);
        }
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

    /// A pane drawn under a scissor blurs only what the scissor needs, and
    /// comes out exactly as the same pixels of the pane drawn whole.
    #[test]
    fn glass_under_a_scissor_matches_the_whole_pane() {
        use crate::glass::Glass;
        let (width, height) = (320u32, 200u32);
        let backdrop: std::vec::Vec<u32> = (0..width * height)
            .map(|index| {
                let (x, y) = (index % width, index / width);
                // Detail the blur has to work on, not just a gradient.
                (x * 255 / width) << 16 | (y * 255 / height) << 8 | ((x ^ y) & 0x3f) * 4
            })
            .collect();
        let info = SurfaceInfo {
            width,
            height,
            pitch: width * 4,
            format: PixelFormat::XRGB8888,
        };
        let rect = Rect::new(60, 50, 200, 80);
        let draw = |scissor: Option<Rect>, glass: Glass| {
            let mut pixels = backdrop.clone();
            // SAFETY: `pixels` outlives the renderer and is `width * height`.
            let mut renderer = unsafe { SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info) }.unwrap();
            renderer.attach_glass_scratch(Box::leak(std::vec![0u32; 1 << 20].into_boxed_slice()));
            let pane = Command::Glass { rect, glass, alpha: 255 };
            match scissor {
                Some(rect) => submit(&mut renderer, &[Command::Scissor { rect }, pane]),
                None => submit(&mut renderer, &[pane]),
            }
            pixels
        };
        for glass in [Glass::regular(true), Glass::regular(false), Glass::clear(true)] {
            let whole = draw(None, glass);
            for piece in [Rect::new(0, 0, 133, 200), Rect::new(133, 0, 187, 200), Rect::new(100, 87, 60, 9)] {
                let part = draw(Some(piece), glass);
                for y in piece.y as u32..(piece.y as u32 + piece.height).min(height) {
                    for x in piece.x as u32..(piece.x as u32 + piece.width).min(width) {
                        let at = (y * width + x) as usize;
                        assert_eq!(part[at], whole[at], "{piece:?} at ({x}, {y})");
                    }
                }
            }
        }
    }

    /// Not a check: how long a title bar's worth of glass takes, with and
    /// without its blur, built optimised. `make glass-bench` runs it.
    #[test]
    #[ignore]
    fn glass_bench() {
        use crate::glass::Glass;
        let (width, height) = (1280u32, 800u32);
        let mut pixels = std::vec![0u32; (width * height) as usize];
        for (index, pixel) in pixels.iter_mut().enumerate() {
            let (x, y) = (index as u32 % width, index as u32 / width);
            *pixel = (x * 255 / width) << 16 | (y * 255 / height) << 8 | 0x60;
        }
        let info = SurfaceInfo {
            width,
            height,
            pitch: width * 4,
            format: PixelFormat::XRGB8888,
        };
        // SAFETY: `pixels` outlives the renderer and is `width * height`.
        let mut renderer = unsafe { SoftwareRenderer::new(pixels.as_mut_ptr().cast(), info) }.unwrap();
        renderer.attach_glass_scratch(Box::leak(std::vec![0u32; 4 << 20].into_boxed_slice()));
        let rect = Rect::new(100, 90, 600, 40);
        let runs = [
            ("regular", Glass::regular(true)),
            ("regular, no frost", Glass { frost: 0, ..Glass::regular(true) }),
            ("clear", Glass::clear(true)),
        ];
        for (name, glass) in runs {
            let start = std::time::Instant::now();
            let rounds = 200;
            for _ in 0..rounds {
                submit(&mut renderer, &[Command::Glass { rect, glass, alpha: 255 }]);
            }
            std::println!("glass-bench: {name:<18} {:>6} us a pane", start.elapsed().as_micros() / rounds);
        }
    }
}
