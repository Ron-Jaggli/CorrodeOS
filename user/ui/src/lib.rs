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
pub use corrode_theme::Theme;
/// The theme's parts, for programs that change it.
pub use corrode_theme as theme;

/// A colour, as a window stores it.
pub type Rgb = u32;

/// `round(x / 255)`, exact for `x <= 65535`.
const fn div255(x: u32) -> u32 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// A theme colour as a window stores it.
pub fn rgb(color: corrode_theme::Color) -> Rgb {
    (color.r as u32) << 16 | (color.g as u32) << 8 | color.b as u32
}

/// A pixel that is not there, in a translucent window: whatever is behind
/// the window shows through it.
pub const CLEAR: Rgb = 0xff00_0000;

/// `src` over `dst` at `alpha` out of 255, channel by channel.
///
/// The top byte of a translucent window's pixel is how transparent it is,
/// so `dst` may be partly there. Its colour then counts for only as much
/// of it as there is, or text drawn on a clear pixel would come out with a
/// dark fringe from the black that is not there.
fn blend(src: Rgb, dst: Rgb, alpha: u32) -> Rgb {
    let there = 255 - (dst >> 24);
    let below = div255(there * (255 - alpha));
    let opacity = alpha + below;
    if opacity == 0 {
        return CLEAR;
    }
    let mut out = (255 - opacity) << 24;
    for shift in [0, 8, 16] {
        let (s, d) = ((src >> shift) & 0xff, (dst >> shift) & 0xff);
        out |= ((s * alpha + d * below + opacity / 2) / opacity).min(255) << shift;
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
    /// The desktop's look: when the window opens, and when it changes.
    Theme(Theme),
    /// To docked windows: the title of the window with the keyboard, empty
    /// if none has it.
    Active([u8; 8]),
    /// The window is a new size, already in effect: `width` and `height`
    /// say what it is now. Everything needs drawing again.
    Resize { width: u32, height: u32 },
    /// To docked windows: the wall clock read `unix` seconds since 1970 UTC
    /// when the monotonic clock read `at` nanoseconds.
    Clock { unix: i64, at: u64 },
    /// The wheel turned `notches` (toward the person positive) with the
    /// pointer at `x`, `y` in the window.
    Scroll { notches: i32, x: i32, y: i32 },
    /// To Files, from search: show the file at this path.
    Reveal([u8; 32]),
    /// A notification was posted: Control Center's list is out of date.
    Notified,
    /// To a popover: the pointer is at x, y on it; -1, -1 when it leaves.
    Hover { x: i32, y: i32 },
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
        Self::open_with(width, height, if docked { window::DOCKED } else { 0 })
    }

    /// As [`Window::open`], with the flags `window::OPEN` takes: docked,
    /// translucent.
    pub fn open_with(width: u32, height: u32, flags: u64) -> Result<Window> {
        Self::open_titled(width, height, flags, "")
    }

    /// As [`Window::open_with`], with a title for the window's title bar:
    /// its first eight ASCII characters.
    pub fn open_titled(width: u32, height: u32, flags: u64, title: &str) -> Result<Window> {
        let mut name = [0u8; 8];
        for (slot, byte) in name.iter_mut().zip(title.bytes().filter(u8::is_ascii)) {
            *slot = byte;
        }
        let answer = corrode_rt::call(
            app::WINDOWS,
            Message::new(
                window::OPEN,
                [
                    width as u64,
                    height as u64,
                    flags,
                    u64::from_le_bytes(name),
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

    /// Fill a rectangle with rounded corners, smoothed at the edges, over
    /// what is there. `color`'s top byte is how transparent it is.
    pub fn rounded(&mut self, x: i32, y: i32, width: u32, height: u32, radius: u32, color: Rgb) {
        let Some((x0, y0, x1, y1)) = self.clip(x, y, width, height) else {
            return;
        };
        let alpha = 255 - (color >> 24);
        let radius = radius.min(width / 2).min(height / 2) as i64;
        // In quarters of a pixel, from the rectangle's corner.
        let (w, h, r) = (width as i64 * 4, height as i64 * 4, radius * 4);
        for row in y0..y1 {
            for column in x0..x1 {
                let (px, py) = (column as i64 - x as i64, row as i64 - y as i64);
                let mut hits = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let (qx, qy) = (px * 4 + sx, py * 4 + sy);
                        // Distance into the nearest corner's square, if in one.
                        let dx = (r - qx).max(qx - (w - 1 - r)).max(0);
                        let dy = (r - qy).max(qy - (h - 1 - r)).max(0);
                        hits += (dx * dx + dy * dy <= r * r) as u32;
                    }
                }
                if hits == 0 {
                    continue;
                }
                // SAFETY: as for `fill`.
                unsafe {
                    let pixel = self.pixels.add(row * self.stride + column);
                    pixel.write(blend(color, pixel.read(), alpha * hits / 16));
                }
            }
        }
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
    /// nanoseconds; `u64::MAX` for ever. A resize takes effect before it is
    /// returned.
    pub fn wait(&mut self, deadline: u64) -> Event {
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
            window::THEME => Event::Theme(Theme::from_words(words)),
            window::ACTIVE => Event::Active(words[0].to_le_bytes()),
            window::HOVER => Event::Hover {
                x: words[0] as i64 as i32,
                y: words[1] as i64 as i32,
            },
            window::SCROLL => Event::Scroll {
                notches: words[0] as i64 as i32,
                x: words[1] as i64 as i32,
                y: words[2] as i64 as i32,
            },
            window::NOTIFIED => Event::Notified,
            window::REVEAL => {
                let mut path = [0u8; 32];
                for (chunk, word) in path.chunks_mut(8).zip(words) {
                    chunk.copy_from_slice(&word.to_le_bytes());
                }
                Event::Reveal(path)
            }
            window::CLOCK => Event::Clock {
                unix: words[0] as i64,
                at: words[1],
            },
            window::RESIZE => {
                // The kernel checked the new size fits the mapping, which
                // never changes.
                self.width = words[0] as u32;
                self.height = words[1] as u32;
                self.stride = self.width as usize;
                Event::Resize {
                    width: self.width,
                    height: self.height,
                }
            }
            _ => Event::Timeout,
        }
    }
}

/// Make `theme` the desktop's look. Every window hears of it as an
/// [`Event::Theme`], this one included.
pub fn set_theme(theme: &Theme) -> Result<()> {
    let answer = corrode_rt::call(
        app::SHELL,
        Message::new(window::SET_THEME, theme.to_words()),
        None,
    )?;
    match (answer.message.label, answer.message.words[0]) {
        (window::DENIED, _) => Err(corrode_rt::Error::Refused),
        (_, 0) => Ok(()),
        _ => Err(corrode_rt::Error::Refused),
    }
}

/// Show a notification saying `text` -- its first 32 bytes -- under this
/// program's window title.
pub fn notify(text: &str) -> Result<()> {
    let mut words = [0u64; 4];
    for (index, byte) in text.bytes().take(32).enumerate() {
        words[index / 8] |= (byte as u64) << (index % 8 * 8);
    }
    corrode_rt::call(app::WINDOWS, Message::new(window::NOTIFY, words), None)?;
    Ok(())
}

/// What the desktop's menu was opened for: the kind (0 the desktop, 1 a
/// dock icon), the program, whether it runs, and its title.
pub fn context() -> [u64; 4] {
    corrode_rt::call(app::SHELL, Message::new(window::CONTEXT, [0; 4]), None)
        .map_or([0; 4], |answer| answer.message.words)
}

/// Ask every window titled `title` to close. How many were asked.
pub fn quit(title: &str) -> u64 {
    let mut word = [0u8; 8];
    for (slot, byte) in word.iter_mut().zip(title.bytes()) {
        *slot = byte;
    }
    corrode_rt::call(
        app::SHELL,
        Message::new(window::QUIT, [u64::from_le_bytes(word), 0, 0, 0]),
        None,
    )
    .map_or(0, |answer| answer.message.words[0])
}

/// A notification from the last few.
pub struct Note {
    pub from: alloc::string::String,
    pub text: alloc::string::String,
    /// How long ago it came, in seconds.
    pub age: u64,
}

/// The most recent notifications, newest first: up to five.
pub fn notifications() -> alloc::vec::Vec<Note> {
    let text = |words: [u64; 4]| -> alloc::string::String {
        words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .take_while(|byte| *byte != 0)
            .map(|byte| byte as char)
            .collect()
    };
    let ask = |which: usize, part: u64| {
        corrode_rt::call(
            app::SHELL,
            Message::new(window::HISTORY, [which as u64, part, 0, 0]),
            None,
        )
        .map(|answer| answer.message.words)
    };
    let mut notes = alloc::vec::Vec::new();
    for which in 0..5 {
        let Ok(head) = ask(which, 0) else { break };
        if which as u64 >= head[0] {
            break;
        }
        let Ok(body) = ask(which, 1) else { break };
        notes.push(Note {
            from: text([head[1], 0, 0, 0]),
            text: text(body),
            age: head[2],
        });
    }
    notes
}

/// Say what this program's windows show, after their title: "Terminal - /etc".
/// Up to 24 bytes; an empty one takes it away.
pub fn subtitle(text: &str) {
    let mut bytes = [0u8; 24];
    for (slot, byte) in bytes.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
    let _ = corrode_rt::call(
        app::WINDOWS,
        Message::new(window::SUBTITLE, [word(0), word(8), word(16), 0]),
        None,
    );
}

/// The subtitles of the windows titled `title`, oldest first: what a dock
/// icon's menu lists.
pub fn windows_titled(title: &str) -> alloc::vec::Vec<alloc::string::String> {
    let mut word = [0u8; 8];
    for (slot, byte) in word.iter_mut().zip(title.bytes()) {
        *slot = byte;
    }
    let title = u64::from_le_bytes(word);
    let mut found = alloc::vec::Vec::new();
    let mut which = 0u64;
    loop {
        let Ok(answer) = corrode_rt::call(app::SHELL, Message::new(window::LIST, [title, which, 0, 0]), None)
        else {
            break;
        };
        let words = answer.message.words;
        if which >= words[0] || which >= 8 {
            break;
        }
        let text: alloc::string::String = words[1..]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .take_while(|byte| *byte != 0)
            .map(|byte| byte as char)
            .collect();
        found.push(text);
        which += 1;
    }
    found
}

/// Bring the `which`th window titled `title` forward.
pub fn raise(title: &str, which: usize) {
    let mut word = [0u8; 8];
    for (slot, byte) in word.iter_mut().zip(title.bytes()) {
        *slot = byte;
    }
    let _ = corrode_rt::call(
        app::SHELL,
        Message::new(window::RAISE, [u64::from_le_bytes(word), which as u64, 0, 0]),
        None,
    );
}

/// Forget the recent notifications.
pub fn clear_notifications() {
    let _ = corrode_rt::call(app::SHELL, Message::new(window::HISTORY, [0, 2, 0, 0]), None);
}

/// Ask for this program's popover to be `width` by `height`. It arrives as
/// an [`Event::Resize`] once the desktop has made it so.
pub fn grow(width: u32, height: u32) -> bool {
    corrode_rt::call(
        app::WINDOWS,
        Message::new(window::GROW, [width as u64, height as u64, 0, 0]),
        None,
    )
    .is_ok_and(|answer| answer.message.words[0] == 1)
}

/// Whether the lock screen has to ask for a passcode.
pub fn passcode_needed() -> bool {
    corrode_rt::call(app::WINDOWS, Message::new(window::UNLOCK, [0; 4]), None)
        .is_ok_and(|answer| answer.message.words[0] == 0)
}

/// Is `code` the passcode? `Ok(true)` or `Ok(false)`, or `Err(seconds)`
/// to wait after too many wrong tries.
pub fn try_passcode(code: &str) -> core::result::Result<bool, u64> {
    let mut bytes = [0u8; 32];
    for (slot, byte) in bytes.iter_mut().zip(code.bytes()) {
        *slot = byte;
    }
    let mut words = [0u64; 4];
    for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*chunk);
    }
    if words == [0; 4] {
        // That would only ask whether there is a passcode.
        return Ok(false);
    }
    match corrode_rt::call(app::WINDOWS, Message::new(window::UNLOCK, words), None) {
        Ok(answer) if answer.message.words[1] > 0 => Err(answer.message.words[1]),
        Ok(answer) => Ok(answer.message.words[0] == 1),
        Err(_) => Ok(false),
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
