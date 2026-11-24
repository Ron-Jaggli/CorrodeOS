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
//!
//! The last row changes the passcode: the current one, then the new one
//! twice, each typed as dots. The desktop checks the current one under the
//! lock screen's limit on tries, and seals the vault on the disk again
//! under the new one (`window::PASSCODE_CHECK`, `PASSCODE_SET`); it takes
//! those calls from Settings alone. Without a vault there is nothing kept
//! to change, and the row says so.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use corrode_rt::abi::app;
use corrode_rt::println;
use corrode_ui::theme::{Buttons, Color, GlassMode, Wallpaper, PRESETS};
use corrode_ui::{line_height, rgb, width_of, Change, Event, Rgb, Style, Theme, Weight, Window, CLEAR};

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
    /// A button that starts changing the passcode, and the place it is
    /// typed while it is being changed.
    Passcode,
}

struct Row {
    label: &'static str,
    control: Control,
}

const fn on_off(value: bool) -> Option<usize> {
    Some(if value { 0 } else { 1 })
}

/// How big a dock icon grows under the pointer at each of Magnify's
/// segments, in percent. Medium is what the presets that magnify use.
const MAGNIFY: [u8; 4] = [100, 120, 140, 170];

const ROWS: [Row; 13] = [
    Row {
        label: "Preset",
        control: Control::Choice {
            options: &["Corrode", "Tahoe", "Light", "Breeze", "Crystal", "Plain"],
            // The clock is where the person is, not part of a look: kept
            // across presets, and not counted in which one this is.
            get: |theme| {
                PRESETS.iter().position(|(_, preset)| {
                    Theme {
                        utc_offset: theme.utc_offset,
                        clock_24h: theme.clock_24h,
                        ..*preset
                    } == *theme
                })
            },
            set: |theme, index| {
                *theme = Theme {
                    utc_offset: theme.utc_offset,
                    clock_24h: theme.clock_24h,
                    ..PRESETS[index].1
                }
            },
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
        label: "Magnify",
        control: Control::Choice {
            options: &["Off", "Small", "Medium", "Large"],
            get: |theme| MAGNIFY.iter().position(|m| *m == theme.magnify),
            set: |theme, index| theme.magnify = MAGNIFY[index],
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
        label: "Clock",
        control: Control::Choice {
            options: &["24-hour", "12-hour"],
            get: |theme| on_off(theme.clock_24h),
            set: |theme, index| theme.clock_24h = index == 0,
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
    Row {
        label: "Passcode",
        control: Control::Passcode,
    },
];

/// Room under the card for what the passcode row last said.
const SAID: i32 = 24;
const HEIGHT: u32 = (HEADER + ROWS.len() as i32 * ROW + 2 * MARGIN + SAID) as u32;

/// Return, Backspace and Escape, by scancode.
const KEY_ENTER: u8 = 0x1c;
const KEY_BACKSPACE: u8 = 0x0e;
const KEY_ESCAPE: u8 = 0x01;
const MOST: usize = 32;
const DOT: u32 = 8;
const MOST_DOTS: usize = 16;

/// Where changing the passcode is.
enum Step {
    Idle,
    Current(String),
    New(String),
    Again { new: String, typed: String },
}

/// The passcode row's state: the step, and what it last said.
struct Passcode {
    step: Step,
    said: Option<&'static str>,
}

impl Passcode {
    fn typed(&mut self) -> Option<&mut String> {
        match &mut self.step {
            Step::Idle => None,
            Step::Current(typed) | Step::New(typed) | Step::Again { typed, .. } => Some(typed),
        }
    }

    /// Return: the next step, asking the desktop where it must.
    fn submit(&mut self) {
        let step = core::mem::replace(&mut self.step, Step::Idle);
        let (step, said) = match step {
            Step::Idle => (Step::Idle, None),
            Step::Current(typed) => match corrode_ui::check_passcode(&typed) {
                Change::Done => (Step::New(String::new()), None),
                Change::Wrong => (Step::Current(String::new()), Some("That is not the passcode")),
                Change::Wait(_) => (Step::Idle, Some("Too many tries - wait, then try again")),
                Change::NoVault => (Step::Idle, Some("No vault on the disk: no passcode is kept to change")),
            },
            Step::New(typed) if !allowed(&typed) => {
                (Step::New(String::new()), Some("4 to 32 characters, no space and no #"))
            }
            Step::New(typed) => (Step::Again { new: typed, typed: String::new() }, None),
            Step::Again { new, typed } if new != typed => {
                (Step::New(String::new()), Some("They differ - type the new one again"))
            }
            Step::Again { new, .. } => match corrode_ui::set_passcode(&new) {
                Change::Done => {
                    println!("the passcode changed");
                    (Step::Idle, Some("Changed. The files on the disk are sealed under it."))
                }
                _ => (Step::Idle, Some("Not changed: the disk did not take it")),
            },
        };
        self.step = step;
        self.said = said;
    }
}

/// What a passcode can hold (`corrode_theme::passcode::allowed`).
fn allowed(text: &str) -> bool {
    (4..=MOST).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'#')
}

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

fn draw(window: &mut Window, theme: &Theme, passcode: &Passcode) {
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
            Control::Passcode => {
                let (x, y, width, height) = control_box(index);
                window.rounded(x, y, width, height, 7, look.track);
                let ty = y + (height as i32 - line_height(SMALL) as i32) / 2;
                let (prompt, count) = match &passcode.step {
                    Step::Idle => ("Change passcode", None),
                    Step::Current(typed) => ("Current", Some(typed.len())),
                    Step::New(typed) => ("New", Some(typed.len())),
                    Step::Again { typed, .. } => ("New, again", Some(typed.len())),
                };
                match count {
                    None => {
                        let ow = width_of(prompt, SMALL);
                        window.text(x + (width as i32 - ow as i32) / 2, ty, prompt, SMALL, look.accent);
                    }
                    Some(count) => {
                        window.text(x + 10, ty, prompt, SMALL, look.secondary);
                        // A dot for each character, never the character.
                        let mut dx = x + 110;
                        let dy = y + (height - DOT) as i32 / 2;
                        for _ in 0..count.min(MOST_DOTS) {
                            window.rounded(dx, dy, DOT, DOT, DOT / 2, look.text);
                            dx += 2 * DOT as i32;
                        }
                    }
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
    if let Some(said) = passcode.said {
        let y = row_top(ROWS.len()) + (SAID - line_height(SMALL) as i32) / 2 + 2;
        window.text(MARGIN + 12, y, said, SMALL, look.secondary);
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
        Control::Passcode => return None,
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
    let mut passcode = Passcode { step: Step::Idle, said: None };
    let passcode_row = ROWS.len() - 1;
    draw(&mut window, &theme, &passcode);
    println!("open, {}x{}", window.width, window.height);

    loop {
        match window.wait(u64::MAX) {
            Event::Theme(now) => {
                theme = now;
                draw(&mut window, &theme, &passcode);
            }
            Event::Button { x, y, buttons } if buttons & 1 != 0 => {
                let (bx, by, bw, bh) = control_box(passcode_row);
                let on_passcode = x >= bx && x < bx + bw as i32 && y >= by && y < by + bh as i32;
                if on_passcode && matches!(passcode.step, Step::Idle) {
                    passcode.step = Step::Current(String::new());
                    passcode.said = None;
                    draw(&mut window, &theme, &passcode);
                } else if let Some(changed) = click(&theme, x, y) {
                    match corrode_ui::set_theme(&changed) {
                        Ok(()) => println!("the look changed"),
                        Err(error) => println!("the look could not change: {:?}", error),
                    }
                }
            }
            Event::Key {
                key: KEY_ENTER,
                pressed: true,
                ..
            } => {
                passcode.submit();
                draw(&mut window, &theme, &passcode);
            }
            Event::Key {
                key: KEY_ESCAPE,
                pressed: true,
                ..
            } => {
                passcode.step = Step::Idle;
                passcode.said = None;
                draw(&mut window, &theme, &passcode);
            }
            Event::Key {
                key: KEY_BACKSPACE,
                pressed: true,
                ..
            } => {
                if let Some(typed) = passcode.typed() {
                    typed.pop();
                    draw(&mut window, &theme, &passcode);
                }
            }
            Event::Key {
                text: Some(character),
                pressed: true,
                ..
            } if character.is_ascii_graphic() && character != '#' => {
                if let Some(typed) = passcode.typed().filter(|typed| typed.len() < MOST) {
                    typed.push(character);
                    draw(&mut window, &theme, &passcode);
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
