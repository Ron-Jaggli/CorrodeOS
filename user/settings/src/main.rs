//! Settings: the desktop's look, changed while you watch.
//!
//! Each row is one of `/etc/desktop.conf`'s settings as a segmented control,
//! or a row of swatches for the accent. A click changes this program's copy
//! of the theme and sends the whole of it to the window server, which tells
//! every window -- this one too. The redraw waits for that echo, so what
//! the window shows is always what the desktop has, not what was asked.
//!
//! It holds nothing the terminal does not: changing the look is something
//! any program with a window may do.

#![no_std]
#![no_main]

extern crate alloc;

use corrode_rt::abi::app;
use corrode_rt::println;
use corrode_ui::theme::{Buttons, Color, GlassMode, Wallpaper, PRESETS};
use corrode_ui::{line_height, rgb, width_of, Event, Rgb, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const WIDTH: u32 = 600;
const MARGIN: i32 = 16;
const HEADER: i32 = 52;
const ROW: i32 = 38;
const LABEL_WIDTH: i32 = 128;

const TITLE: Style = Style::px(20).weight(Weight::Bold);
const LABEL: Style = Style::px(14);
const SMALL: Style = Style::px(13);

/// macOS's accent colours, after the theme's own orange.
const ACCENTS: [Color; 8] = [
    Color::rgb(0xe8, 0x6a, 0x33),
    Color::rgb(0x0a, 0x84, 0xff),
    Color::rgb(0xbf, 0x5a, 0xf2),
    Color::rgb(0xff, 0x37, 0x5f),
    Color::rgb(0xff, 0x45, 0x3a),
    Color::rgb(0xff, 0xd6, 0x0a),
    Color::rgb(0x30, 0xd1, 0x58),
    Color::rgb(0x8e, 0x8e, 0x93),
];

/// One setting.
enum Control {
    /// A segmented control: which segment the theme is at, and what picking
    /// one does to it.
    Choice {
        options: &'static [&'static str],
        get: fn(&Theme) -> Option<usize>,
        set: fn(&mut Theme, usize),
    },
    Accent,
}

struct Row {
    label: &'static str,
    control: Control,
}

const fn on_off(value: bool) -> Option<usize> {
    Some(if value { 0 } else { 1 })
}

const ROWS: [Row; 10] = [
    Row {
        label: "Preset",
        control: Control::Choice {
            options: &["Corrode", "Tahoe", "Light", "Breeze", "Crystal", "Plain"],
            get: |theme| {
                PRESETS
                    .iter()
                    .position(|(_, preset)| preset.to_words() == theme.to_words())
            },
            set: |theme, index| *theme = PRESETS[index].1,
        },
    },
    Row {
        label: "Appearance",
        control: Control::Choice {
            options: &["Dark", "Light"],
            get: |theme| on_off(theme.dark),
            set: |theme, index| theme.dark = index == 0,
        },
    },
    Row {
        label: "Accent",
        control: Control::Accent,
    },
    Row {
        label: "Glass",
        control: Control::Choice {
            options: &["Regular", "Clear", "Off"],
            get: |theme| Some(theme.glass as usize),
            set: |theme, index| {
                theme.glass = [GlassMode::Regular, GlassMode::Clear, GlassMode::Off][index]
            },
        },
    },
    Row {
        label: "Wallpaper",
        control: Control::Choice {
            options: &["Aurora", "Ribbons", "Dunes", "Gradient"],
            get: |theme| Some(theme.wallpaper as usize),
            set: |theme, index| {
                theme.wallpaper = [
                    Wallpaper::Aurora,
                    Wallpaper::Ribbons,
                    Wallpaper::Dunes,
                    Wallpaper::Gradient,
                ][index]
            },
        },
    },
    Row {
        label: "Buttons",
        control: Control::Choice {
            options: &["Left (macOS)", "Right (Plasma)"],
            get: |theme| Some(theme.buttons as usize),
            set: |theme, index| theme.buttons = [Buttons::Left, Buttons::Right][index],
        },
    },
    Row {
        label: "Corners",
        control: Control::Choice {
            options: &["Square", "Small", "Medium", "Large"],
            get: |theme| [0, 6, 12, 18].iter().position(|r| *r == theme.radius),
            set: |theme, index| theme.radius = [0, 6, 12, 18][index],
        },
    },
    Row {
        label: "Dock",
        control: Control::Choice {
            options: &["Show", "Hide"],
            get: |theme| on_off(theme.dock),
            set: |theme, index| theme.dock = index == 0,
        },
    },
    Row {
        label: "Shadows",
        control: Control::Choice {
            options: &["On", "Off"],
            get: |theme| on_off(theme.shadows),
            set: |theme, index| theme.shadows = index == 0,
        },
    },
    Row {
        label: "Motion",
        control: Control::Choice {
            options: &["Off", "Slow", "Normal", "Fast"],
            get: |theme| [0, 60, 100, 160].iter().position(|s| *s == theme.speed),
            set: |theme, index| theme.speed = [0, 60, 100, 160][index],
        },
    },
];

const HEIGHT: u32 = (HEADER + ROWS.len() as i32 * ROW + 2 * MARGIN) as u32;

/// The colours, from the theme.
struct Look {
    /// Under everything: the glass frame shows through a little.
    background: Rgb,
    card: Rgb,
    separator: Rgb,
    text: Rgb,
    secondary: Rgb,
    track: Rgb,
    accent: Rgb,
}

impl Look {
    fn of(theme: &Theme) -> Self {
        let dark = theme.dark;
        Look {
            background: if dark { 0x2018_181a } else { 0x20f2_f2f7 },
            card: if dark { 0x1028_282b } else { 0x10ff_ffff },
            separator: if dark { 0x3a3a_3c } else { 0xd8d8_dc },
            text: rgb(theme.text()),
            secondary: if dark { 0x9898_9f } else { 0x6c6c_70 },
            track: if dark { 0x1c1c_1e } else { 0xe3e3_e8 },
            accent: rgb(theme.accent),
        }
    }
}

/// Where row `index` starts.
fn row_top(index: usize) -> i32 {
    MARGIN + HEADER + index as i32 * ROW
}

/// The control's box in a row: right of the label, to the card's edge less
/// a margin.
fn control_box(index: usize) -> (i32, i32, u32, u32) {
    let left = MARGIN + 12 + LABEL_WIDTH;
    let right = WIDTH as i32 - MARGIN - 12;
    (left, row_top(index) + 7, (right - left) as u32, (ROW - 14) as u32)
}

/// Segment `segment` of `count` in a control box.
fn segment_box(index: usize, segment: usize, count: usize) -> (i32, i32, u32, u32) {
    let (x, y, width, height) = control_box(index);
    let each = width / count as u32;
    (x + (segment as u32 * each) as i32, y, each, height)
}

/// The accent swatches: centres and radius.
fn swatch(index: usize, which: usize) -> (i32, i32, u32) {
    let (x, y, _, height) = control_box(index);
    (x + 12 + which as i32 * 30, y + height as i32 / 2, 9)
}

fn draw(window: &mut Window, theme: &Theme) {
    let look = Look::of(theme);
    window.fill(0, 0, window.width, window.height, CLEAR);
    window.fill(0, 0, window.width, window.height, look.background);

    window.text(MARGIN + 4, MARGIN, "Appearance", TITLE, look.text);
    let note = "Changes apply at once, to every window.";
    window.text(
        MARGIN + 4,
        MARGIN + line_height(TITLE) as i32 + 4,
        note,
        SMALL,
        look.secondary,
    );

    let card_top = row_top(0);
    window.rounded(
        MARGIN,
        card_top,
        WIDTH - 2 * MARGIN as u32,
        (ROWS.len() as i32 * ROW) as u32,
        12,
        look.card,
    );
    for (index, row) in ROWS.iter().enumerate() {
        let top = row_top(index);
        if index > 0 {
            window.fill(
                MARGIN + 12,
                top,
                WIDTH - 2 * MARGIN as u32 - 24,
                1,
                look.separator,
            );
        }
        let text_y = top + (ROW - line_height(LABEL) as i32) / 2;
        window.text(MARGIN + 12, text_y, row.label, LABEL, look.text);

        match &row.control {
            Control::Choice { options, get, .. } => {
                let (x, y, width, height) = control_box(index);
                window.rounded(x, y, width, height, 7, look.track);
                let current = get(theme);
                for (segment, option) in options.iter().enumerate() {
                    let (sx, sy, sw, sh) = segment_box(index, segment, options.len());
                    let chosen = current == Some(segment);
                    if chosen {
                        window.rounded(sx + 2, sy + 2, sw - 4, sh - 4, 6, look.accent);
                    }
                    let ow = width_of(option, SMALL);
                    let color = if chosen { 0xff_ffff } else { look.text };
                    window.text(
                        sx + (sw as i32 - ow as i32) / 2,
                        sy + (sh as i32 - line_height(SMALL) as i32) / 2,
                        option,
                        SMALL,
                        color,
                    );
                }
            }
            Control::Accent => {
                for (which, color) in ACCENTS.iter().enumerate() {
                    let (cx, cy, r) = swatch(index, which);
                    if *color == theme.accent {
                        let ring = r + 3;
                        window.rounded(cx - ring as i32, cy - ring as i32, 2 * ring, 2 * ring, ring, look.text);
                        let gap = r + 1;
                        window.rounded(cx - gap as i32, cy - gap as i32, 2 * gap, 2 * gap, gap, look.track);
                    }
                    window.rounded(cx - r as i32, cy - r as i32, 2 * r, 2 * r, r, rgb(*color));
                }
            }
        }
    }
    window.commit_all();
}

/// What a click at `x`, `y` makes of `theme`, if anything.
fn click(theme: &Theme, x: i32, y: i32) -> Option<Theme> {
    let index = ROWS
        .iter()
        .enumerate()
        .position(|(index, _)| y >= row_top(index) && y < row_top(index) + ROW)?;
    let mut changed = *theme;
    match &ROWS[index].control {
        Control::Choice { options, set, .. } => {
            let segment = (0..options.len()).find(|segment| {
                let (sx, sy, sw, sh) = segment_box(index, *segment, options.len());
                x >= sx && x < sx + sw as i32 && y >= sy && y < sy + sh as i32
            })?;
            set(&mut changed, segment);
        }
        Control::Accent => {
            let which = (0..ACCENTS.len()).find(|which| {
                let (cx, cy, r) = swatch(index, *which);
                let reach = r as i32 + 4;
                (x - cx).abs() <= reach && (y - cy).abs() <= reach
            })?;
            changed.accent = ACCENTS[which];
        }
    }
    (changed != *theme).then_some(changed)
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let mut window = match Window::open_titled(WIDTH, HEIGHT, corrode_rt::abi::window::TRANSLUCENT, "Settings") {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut theme = Theme::default();
    draw(&mut window, &theme);
    println!("open, {}x{}", window.width, window.height);

    loop {
        match window.wait(u64::MAX) {
            Event::Theme(now) => {
                theme = now;
                draw(&mut window, &theme);
            }
            Event::Button { x, y, buttons } if buttons & 1 != 0 => {
                if let Some(changed) = click(&theme, x, y) {
                    match corrode_ui::set_theme(&changed) {
                        Ok(()) => println!("the look changed"),
                        Err(error) => println!("the look could not change: {:?}", error),
                    }
                }
            }
            Event::Close => {
                println!("closed");
                corrode_rt::exit();
            }
            _ => {}
        }
    }
}
