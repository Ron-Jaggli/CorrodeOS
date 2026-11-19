//! What a desktop program needs to have a window: ask for one, draw
//! rectangles and text in it, say what changed, and hear what happens to it.
//!
//! Text comes from the same engine the boot screen uses: it draws coverage,
//! and this puts a colour through it onto the window's pixels. The window's
//! pixels are `0x00RRGGBB`, whatever the screen is.

#![no_std]

extern crate alloc;

use alloc::vec;

use corrode_rt::abi::{app, window};
use corrode_rt::{Message, Result};
pub use corrode_text::{Style, Weight};

/// A colour, as a window stores it.
pub type Rgb = u32;

/// `round(x / 255)`, exact for `x <= 65535`.
const fn div255(x: u32) -> u32 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// `src` over `dst` at `alpha` out of 255, channel by channel.
fn blend(src: Rgb, dst: Rgb, alpha: u32) -> Rgb {
    let mut out = 0;
    for shift in [0, 8, 16] {
        let (s, d) = ((src >> shift) & 0xff, (dst >> shift) & 0xff);
        out |= div255(s * alpha + d * (255 - alpha)) << shift;
    }
    out
}

/// Something that happened to a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key, while this window has the keyboard.
    Key {
        key: u8,
        pressed: bool,
        text: Option<char>,
        modifiers: u8,
    },
    /// A mouse button changed over the window, at `x`, `y` from its corner.
    Button { x: i32, y: i32, buttons: u8 },
    /// The window gained or lost the keyboard.
    Focus(bool),
    /// The close button on the title bar was clicked.
    Close,
    /// The deadline came before anything else did.
    Timeout,
}

/// A window, and its pixels mapped into this program.
pub struct Window {
    pixels: *mut u32,
    pub width: u32,
    pub height: u32,
    /// Pixels from one row to the next.
    stride: usize,
}

impl Window {
    /// Ask the window server for a window. It lands in `app::WINDOW`.
    /// `docked` asks for a panel along the top of the screen instead, as wide
    /// as the screen whatever `width` says.
    pub fn open(width: u32, height: u32, docked: bool) -> Result<Window> {
        let answer = corrode_rt::call(
            app::WINDOWS,
            Message::new(
                window::OPEN,
                [
                    width as u64,
                    height as u64,
                    if docked { window::DOCKED } else { 0 },
                    0,
                ],
            ),
            Some(app::WINDOW),
        )?;
        if answer.message.words[0] == 0 {
            return Err(corrode_rt::Error::NoSpace);
        }
        let mapping = corrode_rt::map_surface(app::WINDOW)?;
        Ok(Window {
            pixels: mapping.address,
            width: mapping.width,
            height: mapping.height,
            stride: mapping.pitch as usize / 4,
        })
    }

    /// Fill a rectangle, clipped to the window.
    pub fn fill(&mut self, x: i32, y: i32, width: u32, height: u32, color: Rgb) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, width, height) else {
            return;
        };
        for row in y0..y1 {
            for column in x0..x1 {
                // SAFETY: `clip` kept the pixel inside the window, which the
                // kernel mapped writable at `pixels` for `stride * height`.
                unsafe { self.pixels.add(row * self.stride + column).write(color) };
            }
        }
    }

    fn clip(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Option<(usize, usize, usize, usize)> {
        let x0 = (x as i64).clamp(0, self.width as i64);
        let y0 = (y as i64).clamp(0, self.height as i64);
        let x1 = (x as i64 + width as i64).clamp(0, self.width as i64);
        let y1 = (y as i64 + height as i64).clamp(0, self.height as i64);
        (x0 < x1 && y0 < y1).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    /// Draw `text` with its top left at `x`, `y`, and return how wide it
    /// was.
    pub fn text(&mut self, x: i32, y: i32, text: &str, style: Style, color: Rgb) -> u32 {
        let extent = corrode_text::measure(text, style);
        if extent.area() == 0 {
            return extent.width;
        }
        let mut coverage = vec![0u8; extent.area()];
        corrode_text::draw(
            text,
            style,
            &mut corrode_text::Coverage::new(&mut coverage, extent.width as usize),
        );
        let Some((x0, y0, x1, y1)) = self.clip(x, y, extent.width, extent.height) else {
            return extent.width;
        };
        for row in y0..y1 {
            for column in x0..x1 {
                let covered = coverage[(row as i64 - y as i64) as usize * extent.width as usize
                    + (column as i64 - x as i64) as usize] as u32;
                if covered == 0 {
                    continue;
                }
                // SAFETY: as for `fill`.
                unsafe {
                    let pixel = self.pixels.add(row * self.stride + column);
                    pixel.write(blend(color, pixel.read(), covered));
                }
            }
        }
        extent.width
    }

    /// Say that a rectangle has been redrawn, so it gets shown.
    pub fn commit(&self, x: i32, y: i32, width: u32, height: u32) {
        let _ = corrode_rt::commit(app::WINDOW, x, y, width, height);
    }

    /// Say that all of it has.
    pub fn commit_all(&self) {
        self.commit(0, 0, self.width, self.height);
    }

    /// Wait for something to happen, or for the clock to read `deadline`
    /// nanoseconds; `u64::MAX` for ever.
    pub fn wait(&self, deadline: u64) -> Event {
        let Ok(Some(message)) = corrode_rt::wait(app::WINDOW, deadline) else {
            return Event::Timeout;
        };
        let words = message.words;
        match message.label {
            window::KEY => Event::Key {
                key: words[0] as u8,
                pressed: words[1] != 0,
                text: u32::try_from(words[2])
                    .ok()
                    .filter(|code| *code != 0)
                    .and_then(char::from_u32),
                modifiers: words[3] as u8,
            },
            window::BUTTON => Event::Button {
                x: words[0] as i32,
                y: words[1] as i32,
                buttons: words[2] as u8,
            },
            window::FOCUS => Event::Focus(words[0] != 0),
            window::CLOSE => Event::Close,
            _ => Event::Timeout,
        }
    }
}

/// How tall a line of `style` is.
pub fn line_height(style: Style) -> u32 {
    corrode_text::measure("Mg", style).height
}

/// How wide `text` is in `style`.
pub fn width_of(text: &str, style: Style) -> u32 {
    corrode_text::measure(text, style).width
}
