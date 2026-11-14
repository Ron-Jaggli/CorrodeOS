//! The boot log, on the firmware's framebuffer, for real hardware.
//!
//! QEMU gives us COM1 on stdout. A real desktop usually gives us nothing: no
//! serial header wired up, and the only screen is whatever the GPU's own
//! firmware (GOP) left lit. So under the `hw-log` feature every byte that
//! goes to COM1 also comes here and is drawn, line by line, onto that screen,
//! where a phone camera can read it.
//!
//! Two modes, because the log has to be useful whether the boot finishes or
//! not:
//!
//! * **Live**, while booting: each finished line is drawn in the next row,
//!   wrapping back to the top when the screen is full. The row after the
//!   newest line is cleared and marked with an orange bar, so after a hang
//!   the last thing printed is the line just above the bar.
//! * **Settled**, once [`settle`] is called: the screen is redrawn with the
//!   newest lines in order, top to bottom, which is the one to photograph.
//!
//! Lines that say FAIL or PANIC are drawn red, NVIDIA lines green.
//!
//! Every entry point is called from `serial::_print` with the console lock
//! held and interrupts off, which is what makes the `static mut` state here
//! single-writer. Nothing here allocates.

use crate::limine;
use corrode_text::{Coverage, Style};

/// Glyph height, in pixels. Big enough to read off a photo of a 27" screen.
const STYLE: Style = Style::px(18);
/// Bytes kept of each line; the rest is cut. The widest boot lines are the
/// task tables, which are not what anyone photographs this for.
const LINE_MAX: usize = 160;
/// Lines remembered for [`settle`]. More than any screen holds.
const HISTORY: usize = 128;
/// Widest framebuffer drawn on. Wider ones use their left 4096 pixels.
const MAX_WIDTH: usize = 4096;
/// Tallest a line's coverage block may be.
const MAX_LINE_HEIGHT: usize = 40;
const MARGIN: usize = 12;

const BACKGROUND: (u8, u8, u8) = (0x10, 0x10, 0x14);
const NORMAL: (u8, u8, u8) = (0xd8, 0xd8, 0xd8);
const FAIL: (u8, u8, u8) = (0xff, 0x55, 0x55);
const NVIDIA: (u8, u8, u8) = (0x76, 0xd9, 0x00);
const MARKER: (u8, u8, u8) = (0xff, 0x9f, 0x1c);

struct Screen {
    address: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    shifts: (u8, u8, u8),
    line_height: usize,
    rows: usize,
}

struct State {
    line: [u8; LINE_MAX],
    len: usize,
    /// Next row the live log draws in.
    row: usize,
    history: [[u8; LINE_MAX]; HISTORY],
    history_len: [u8; HISTORY],
    /// Lines ever finished; the newest is at `(written - 1) % HISTORY`.
    written: usize,
}

/// `None` until the first write; `Some(None)` if there is nothing to draw on.
/// Set once, then only read.
static mut SCREEN: Option<Option<Screen>> = None;

static mut STATE: State = State {
    line: [0; LINE_MAX],
    len: 0,
    row: 0,
    history: [[0; LINE_MAX]; HISTORY],
    history_len: [0; HISTORY],
    written: 0,
};

/// One line's coverage, reused for every line.
static mut COVERAGE: [u8; MAX_WIDTH * MAX_LINE_HEIGHT] = [0; MAX_WIDTH * MAX_LINE_HEIGHT];

fn find_screen() -> Option<Screen> {
    let fb = limine::FRAMEBUFFER.response.get()?.first()?;
    // Packed 32-bit RGB with 8-bit channels is what every GOP we care about
    // hands over. Anything else gets no screen log, not a wrong-coloured one.
    if fb.memory_model != limine::MEMORY_MODEL_RGB
        || fb.bpp != 32
        || fb.red_mask_size != 8
        || fb.green_mask_size != 8
        || fb.blue_mask_size != 8
    {
        return None;
    }
    let line_height = corrode_text::measure("Ag", STYLE).height as usize;
    if line_height == 0 || line_height > MAX_LINE_HEIGHT {
        return None;
    }
    let height = fb.height as usize;
    let rows = height / line_height;
    if rows < 2 {
        return None;
    }
    Some(Screen {
        address: fb.address,
        width: (fb.width as usize).min(MAX_WIDTH),
        height,
        pitch: fb.pitch as usize,
        shifts: (fb.red_mask_shift, fb.green_mask_shift, fb.blue_mask_shift),
        line_height,
        rows,
    })
}

impl Screen {
    fn pixel(&self, (r, g, b): (u8, u8, u8)) -> u32 {
        (r as u32) << self.shifts.0 | (g as u32) << self.shifts.1 | (b as u32) << self.shifts.2
    }

    /// Fill rows `y..y + h` of the screen.
    fn fill(&self, y: usize, h: usize, color: (u8, u8, u8)) {
        let value = self.pixel(color);
        for row in y..(y + h).min(self.height) {
            for x in 0..self.width {
                // SAFETY: `row < height` and `x < width <= fb width`, and Limine
                // maps `pitch * height` bytes at `address`.
                unsafe {
                    let at = self.address.add(row * self.pitch + x * 4) as *mut u32;
                    at.write_volatile(value);
                }
            }
        }
    }

    /// Draw `text` across the whole of screen row `row`.
    fn draw(&self, row: usize, text: &[u8]) {
        let text = core::str::from_utf8(text).unwrap_or("<not utf-8>");
        let color = colour_of(text);
        let width = self.width - MARGIN;
        let height = self.line_height;
        // SAFETY: only reached with the console lock held (see the module
        // docs), so nothing else is using the buffer.
        let buffer = unsafe { &mut (&mut *core::ptr::addr_of_mut!(COVERAGE))[..width * height] };
        buffer.fill(0);
        corrode_text::draw(text, STYLE, &mut Coverage::new(buffer, width));

        let top = row * height;
        let background = self.pixel(BACKGROUND);
        for y in 0..height {
            let screen_y = top + y;
            if screen_y >= self.height {
                break;
            }
            // SAFETY: as in `fill`.
            let line = unsafe { self.address.add(screen_y * self.pitch) as *mut u32 };
            for x in 0..self.width {
                let value = if x < MARGIN {
                    background
                } else {
                    match buffer[y * width + (x - MARGIN)] {
                        0 => background,
                        cover => self.pixel(mix(BACKGROUND, color, cover)),
                    }
                };
                // SAFETY: `x < width`, row in bounds, as above.
                unsafe { line.add(x).write_volatile(value) };
            }
        }
    }

    /// The bar that says "the newest line is just above me".
    fn mark(&self, row: usize) {
        let top = row * self.line_height;
        self.fill(top, self.line_height, BACKGROUND);
        self.fill(top + 1, 3, MARKER);
    }
}

fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn colour_of(text: &str) -> (u8, u8, u8) {
    if contains(text, "FAIL") || contains(text, "PANIC") || contains(text, "fault") {
        FAIL
    } else if text.starts_with("nvidia") {
        NVIDIA
    } else {
        NORMAL
    }
}

fn mix(from: (u8, u8, u8), to: (u8, u8, u8), amount: u8) -> (u8, u8, u8) {
    let a = amount as u32;
    let channel = |f: u8, t: u8| ((f as u32 * (255 - a) + t as u32 * a + 127) / 255) as u8;
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

/// The state, if there is a screen to draw it on.
///
/// # Safety
/// Console lock held; see the module docs.
unsafe fn state() -> Option<(&'static mut State, &'static Screen)> {
    // SAFETY: the caller holds the console lock, so these are the only
    // references to either static; `SCREEN` is written here once and only
    // read afterwards.
    unsafe {
        let screen = &mut *core::ptr::addr_of_mut!(SCREEN);
        if screen.is_none() {
            let found = find_screen();
            if let Some(found) = &found {
                found.fill(0, found.height, BACKGROUND);
            }
            *screen = Some(found);
        }
        let screen = (*core::ptr::addr_of!(SCREEN)).as_ref()?.as_ref()?;
        Some((&mut *core::ptr::addr_of_mut!(STATE), screen))
    }
}

/// Take bytes headed for COM1.
///
/// # Safety
/// Only from `serial::_print`, with the console lock held.
pub unsafe fn write(text: &str) {
    // SAFETY: passed through from the caller.
    let Some((state, screen)) = (unsafe { state() }) else {
        return;
    };
    for &byte in text.as_bytes() {
        match byte {
            b'\n' => finish_line(state, screen),
            b'\r' => {}
            b'\t' => push(state, b' '),
            byte => push(state, byte),
        }
    }
}

fn push(state: &mut State, byte: u8) {
    if state.len < LINE_MAX {
        state.line[state.len] = byte;
        state.len += 1;
    }
}

fn finish_line(state: &mut State, screen: &Screen) {
    let len = state.len;
    // Keep whole UTF-8 characters only: a cut through one would not draw.
    let mut keep = len;
    while keep > 0 && core::str::from_utf8(&state.line[..keep]).is_err() {
        keep -= 1;
    }
    let slot = state.written % HISTORY;
    state.history[slot][..keep].copy_from_slice(&state.line[..keep]);
    state.history_len[slot] = keep as u8;
    state.written += 1;
    state.len = 0;

    screen.draw(state.row, &state.line[..keep]);
    state.row = (state.row + 1) % screen.rows;
    screen.mark(state.row);
}

/// Redraw the screen with the newest lines in order, oldest at the top.
///
/// # Safety
/// Only from `serial::settle_screen`, with the console lock held.
pub unsafe fn settle() {
    // SAFETY: passed through from the caller.
    let Some((state, screen)) = (unsafe { state() }) else {
        return;
    };
    let shown = state.written.min(screen.rows - 1).min(HISTORY);
    let first = state.written - shown;
    for (row, index) in (first..state.written).enumerate() {
        let slot = index % HISTORY;
        let len = state.history_len[slot] as usize;
        screen.draw(row, &state.history[slot][..len]);
    }
    for row in shown..screen.rows {
        screen.fill(row * screen.line_height, screen.line_height, BACKGROUND);
    }
    state.row = shown % screen.rows;
    screen.mark(state.row);
}
