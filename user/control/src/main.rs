//! Control Center: the menu bar's quick settings, hung from its clock.
//!
//! A popover of tiles, as macOS's Control Center and Plasma's system tray
//! have: Dark Mode, Glass, the dock, shadows and motion, each a click to
//! turn on or off, the accent colours below them, and a way into Settings
//! for the rest. Changes apply at once, as Settings' do -- this is a
//! shortcut to the same theme, not a second copy of it.
//!
//! The desktop closes it when anything else is clicked.

#![no_std]
#![no_main]

extern crate alloc;

use corrode_rt::abi::{app, launcher, window as flags};
use corrode_rt::{println, Message};
use corrode_ui::theme::{Color, GlassMode};
use corrode_ui::{line_height, rgb, width_of, Event, Note, Rgb, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const MARGIN: i32 = 12;
const GAP: i32 = 8;
const TILE: (i32, i32) = (138, 56);
const COLUMNS: i32 = 2;
const ROWS: i32 = 4;
const SWATCHES: i32 = 44;
const WIDTH: u32 = (2 * MARGIN + COLUMNS * TILE.0 + (COLUMNS - 1) * GAP) as u32;
const HEIGHT: u32 = (2 * MARGIN + ROWS * TILE.1 + ROWS * GAP + SWATCHES) as u32;
/// The notifications card: a heading, then a row for each of the last few
/// (or one saying there are none).
const HEADING: i32 = 30;
const NOTE_ROW: i32 = 40;
const NOTES_SHOWN: usize = 3;

/// How tall the card of notifications is, for `count` of them.
fn notes_height(count: usize) -> i32 {
    HEADING + NOTE_ROW * count.clamp(1, NOTES_SHOWN) as i32 + 6
}

/// "now", "5m ago", "2h ago": how long ago, briefly.
fn ago(seconds: u64) -> alloc::string::String {
    match seconds {
        0..=59 => alloc::string::String::from("now"),
        60..=3599 => alloc::format!("{}m ago", seconds / 60),
        _ => alloc::format!("{}h ago", seconds / 3600),
    }
}

/// The card of recent notifications, under the swatches.
fn notes(window: &mut Window, look: &Look, notes: &[Note]) {
    let top = HEIGHT as i32 - MARGIN + GAP;
    let width = window.width - 2 * MARGIN as u32;
    window.rounded(MARGIN, top, width, notes_height(notes.len()) as u32, 14, look.tile);
    window.text(MARGIN + 12, top + 9, "Notifications", LABEL, look.text);
    let right = MARGIN + width as i32 - 12;
    if notes.is_empty() {
        window.text(MARGIN + 12, top + HEADING + 10, "Nothing new", STATE, look.secondary);
        return;
    }
    for (index, note) in notes.iter().take(NOTES_SHOWN).enumerate() {
        let y = top + HEADING + index as i32 * NOTE_ROW;
        if index > 0 {
            window.fill(MARGIN + 12, y - 3, width - 24, 1, look.off);
        }
        window.rounded(MARGIN + 12, y + 6, 8, 8, 4, look.accent);
        window.text(MARGIN + 28, y + 1, &note.from, LABEL, look.text);
        let when = ago(note.age);
        window.text(right - width_of(&when, STATE) as i32, y + 3, &when, STATE, look.secondary);
        window.text(MARGIN + 28, y + 1 + line_height(LABEL) as i32, &note.text, STATE, look.secondary);
    }
}

const LABEL: Style = Style::px(13).weight(Weight::Bold);
const STATE: Style = Style::px(11);

/// The same eight as Settings offers.
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

/// A tile: what it says, whether it is on, and what clicking it does.
struct Tile {
    label: &'static str,
    on: fn(&Theme) -> bool,
    states: [&'static str; 2],
    flip: fn(&mut Theme),
    icon: Icon,
}

#[derive(Clone, Copy)]
enum Icon {
    Moon,
    Pane,
    Dock,
    Shadow,
    Motion,
    Clock,
    Lock,
    Gear,
}

const TILES: [Tile; 8] = [
    Tile {
        label: "Dark Mode",
        on: |theme| theme.dark,
        states: ["Off", "On"],
        flip: |theme| theme.dark = !theme.dark,
        icon: Icon::Moon,
    },
    Tile {
        label: "Glass",
        on: |theme| theme.glass != GlassMode::Off,
        states: ["Off", "On"],
        flip: |theme| {
            theme.glass = match theme.glass {
                GlassMode::Off => GlassMode::Regular,
                _ => GlassMode::Off,
            }
        },
        icon: Icon::Pane,
    },
    Tile {
        label: "Dock",
        on: |theme| theme.dock,
        states: ["Hidden", "Shown"],
        flip: |theme| theme.dock = !theme.dock,
        icon: Icon::Dock,
    },
    Tile {
        label: "Shadows",
        on: |theme| theme.shadows,
        states: ["Off", "On"],
        flip: |theme| theme.shadows = !theme.shadows,
        icon: Icon::Shadow,
    },
    Tile {
        label: "Motion",
        on: |theme| theme.speed > 0,
        states: ["Off", "On"],
        flip: |theme| theme.speed = if theme.speed > 0 { 0 } else { 100 },
        icon: Icon::Motion,
    },
    Tile {
        label: "24-Hour",
        on: |theme| theme.clock_24h,
        states: ["12-hour clock", "24-hour clock"],
        flip: |theme| theme.clock_24h = !theme.clock_24h,
        icon: Icon::Clock,
    },
    // Not toggles: the lock screen, and a way into the rest.
    Tile {
        label: "Lock",
        on: |_| false,
        states: ["Alt+L", "Alt+L"],
        flip: |_| {},
        icon: Icon::Lock,
    },
    Tile {
        label: "Settings...",
        on: |_| false,
        states: ["All options", "All options"],
        flip: |_| {},
        icon: Icon::Gear,
    },
];

struct Look {
    tile: Rgb,
    text: Rgb,
    secondary: Rgb,
    off: Rgb,
    accent: Rgb,
}

impl Look {
    fn of(theme: &Theme) -> Self {
        let dark = theme.dark;
        Look {
            tile: if dark { 0x6030_3034 } else { 0x40ff_ffff },
            text: rgb(theme.text()),
            secondary: if dark { 0x9898_9f } else { 0x6c6c_70 },
            off: if dark { 0x4848_4c } else { 0xd0d0_d6 },
            accent: rgb(theme.accent),
        }
    }
}

fn tile_at(index: usize) -> (i32, i32) {
    let (column, row) = (index as i32 % COLUMNS, index as i32 / COLUMNS);
    (MARGIN + column * (TILE.0 + GAP), MARGIN + row * (TILE.1 + GAP))
}

fn swatch_at(index: usize) -> (i32, i32) {
    let top = MARGIN + ROWS * (TILE.1 + GAP);
    let step = (WIDTH as i32 - 2 * MARGIN - 24) / (ACCENTS.len() as i32 - 1);
    (MARGIN + 12 + index as i32 * step, top + SWATCHES / 2)
}

/// A tile's icon, white, centred on `x`, `y`.
fn icon(window: &mut Window, icon: Icon, x: i32, y: i32, back: Rgb) {
    let white = 0xff_ffff;
    match icon {
        Icon::Moon => {
            window.rounded(x - 7, y - 7, 14, 14, 7, white);
            window.rounded(x - 3, y - 10, 14, 14, 7, back);
        }
        Icon::Pane => {
            window.rounded(x - 8, y - 7, 16, 14, 4, white);
            window.rounded(x - 6, y - 5, 12, 10, 3, back);
            window.fill(x - 4, y - 3, 3, 1, white);
        }
        Icon::Dock => {
            window.rounded(x - 9, y + 2, 18, 6, 3, white);
            for step in 0..3 {
                window.rounded(x - 7 + step * 5, y - 4, 4, 4, 1, white);
            }
        }
        Icon::Shadow => {
            window.rounded(x - 5, y - 4, 12, 11, 2, 0x8000_0000 | white);
            window.rounded(x - 8, y - 7, 12, 11, 2, white);
        }
        Icon::Motion => {
            for step in 0..3 {
                window.rounded(x - 9 + step * 6, y - 1, 5, 3, 1, white);
            }
            window.rounded(x + 6, y - 5, 4, 11, 2, white);
        }
        Icon::Clock => {
            window.rounded(x - 8, y - 8, 16, 16, 8, white);
            window.rounded(x - 6, y - 6, 12, 12, 6, back);
            window.fill(x, y - 5, 1, 6, white);
            window.fill(x, y, 4, 1, white);
        }
        Icon::Lock => {
            window.rounded(x - 5, y - 9, 10, 12, 5, white);
            window.rounded(x - 3, y - 7, 6, 10, 3, back);
            window.rounded(x - 7, y - 2, 14, 11, 2, white);
        }
        Icon::Gear => {
            window.rounded(x - 8, y - 8, 16, 16, 8, white);
            window.rounded(x - 4, y - 4, 8, 8, 4, back);
        }
    }
}

fn draw(window: &mut Window, theme: &Theme, recent: &[Note]) {
    let look = Look::of(theme);
    let (width, height) = (window.width, window.height);
    window.fill(0, 0, width, height, CLEAR);
    for (index, tile) in TILES.iter().enumerate() {
        let (x, y) = tile_at(index);
        window.rounded(x, y, TILE.0 as u32, TILE.1 as u32, 14, look.tile);
        let on = (tile.on)(theme);
        // The round button: the accent when on, as macOS's are.
        let circle = if on || matches!(tile.icon, Icon::Gear | Icon::Lock) {
            look.accent
        } else {
            look.off
        };
        let (cx, cy) = (x + 10 + 16, y + TILE.1 / 2);
        window.rounded(cx - 16, cy - 16, 32, 32, 16, circle);
        icon(window, tile.icon, cx, cy, circle);
        let text_x = x + 10 + 32 + 10;
        let label_y = y + TILE.1 / 2 - line_height(LABEL) as i32 + 1;
        window.text(text_x, label_y, tile.label, LABEL, look.text);
        let state = tile.states[on as usize];
        window.text(text_x, label_y + line_height(LABEL) as i32, state, STATE, look.secondary);
    }
    // The accents, on a strip of their own.
    let top = MARGIN + ROWS * (TILE.1 + GAP);
    window.rounded(MARGIN, top, width - 2 * MARGIN as u32, SWATCHES as u32, 14, look.tile);
    for (index, color) in ACCENTS.iter().enumerate() {
        let (x, y) = swatch_at(index);
        if *color == theme.accent {
            window.rounded(x - 11, y - 11, 22, 22, 11, look.text);
        }
        window.rounded(x - 9, y - 9, 18, 18, 9, rgb(*color));
    }
    notes(window, &look, recent);
    window.commit_all();
}

/// What a click at `x`, `y` does: a new theme, or a program started.
enum Action {
    Theme(Theme),
    Launch(u64),
}

fn click(theme: &Theme, x: i32, y: i32) -> Option<Action> {
    for (index, tile) in TILES.iter().enumerate() {
        let (left, top) = tile_at(index);
        if x >= left && x < left + TILE.0 && y >= top && y < top + TILE.1 {
            match tile.icon {
                Icon::Gear => return Some(Action::Launch(launcher::SETTINGS)),
                Icon::Lock => return Some(Action::Launch(launcher::LOCK)),
                _ => {}
            }
            let mut changed = *theme;
            (tile.flip)(&mut changed);
            println!("{} turned {}", tile.label, tile.states[(tile.on)(&changed) as usize]);
            return Some(Action::Theme(changed));
        }
    }
    for (index, color) in ACCENTS.iter().enumerate() {
        let (cx, cy) = swatch_at(index);
        if (x - cx).abs() <= 11 && (y - cy).abs() <= 11 {
            return Some(Action::Theme(Theme {
                accent: *color,
                ..*theme
            }));
        }
    }
    None
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    // What has been said lately, asked once: Control Center is opened
    // afresh each time it is wanted.
    let recent = corrode_ui::notifications();
    println!("{} recent notifications", recent.len());
    let mut window = match Window::open_titled(
        WIDTH,
        (HEIGHT as i32 + GAP + notes_height(recent.len())) as u32,
        flags::POPOVER | flags::TRANSLUCENT,
        "Control",
    ) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut theme = Theme::default();
    draw(&mut window, &theme, &recent);
    println!("open");

    loop {
        match window.wait(u64::MAX) {
            Event::Theme(now) => {
                theme = now;
                draw(&mut window, &theme, &recent);
            }
            Event::Button { x, y, buttons } if buttons & 1 != 0 => match click(&theme, x, y) {
                Some(Action::Theme(changed)) => {
                    if let Err(error) = corrode_ui::set_theme(&changed) {
                        println!("the look could not change: {:?}", error);
                    }
                }
                Some(Action::Launch(program)) => {
                    let asked = corrode_rt::call(
                        app::LAUNCHER,
                        Message::new(launcher::LAUNCH, [program, 0, 0, 0]),
                        None,
                    );
                    println!("asked for program {}: {}", program, if asked.is_ok() { "yes" } else { "no" });
                    // Out of the way, as Control Center goes once used.
                    corrode_rt::exit();
                }
                None => {}
            },
            Event::Close => {
                println!("closed");
                corrode_rt::exit();
            }
            _ => {}
        }
    }
}
