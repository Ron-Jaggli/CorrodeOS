//! The desktop's right-click menu: a glass list at the pointer, as
//! macOS's desktop menu and Plasma's are.
//!
//! The item under the pointer is lit in the accent; clicking it, or
//! Return on the one picked with the arrows, does it and closes the menu.
//! The desktop closes it when anything else is clicked.

#![no_std]
#![no_main]

extern crate alloc;

use corrode_rt::abi::{app, launcher, window as flags};
use corrode_rt::{println, Message};
use corrode_ui::theme::Wallpaper;
use corrode_ui::{line_height, rgb, Event, Rgb, Style, Theme, Window, CLEAR};

corrode_rt::entry!(main);

const WIDTH: u32 = 210;
const PAD: i32 = 6;
const ROW: i32 = 26;
const RULE: i32 = 9;
const TEXT: Style = Style::px(13);

const KEY_ESCAPE: u8 = 0x01;
const KEY_ENTER: u8 = 0x1c;
const KEY_UP: u8 = 0x80 | 0x48;
const KEY_DOWN: u8 = 0x80 | 0x50;

#[derive(Clone, Copy)]
enum Item {
    Start(&'static str, u64),
    Wallpaper,
    Appearance,
    Rule,
}

const ITEMS: [Item; 9] = [
    Item::Start("New Terminal", launcher::TERMINAL),
    Item::Start("Open Files", launcher::FILES),
    Item::Rule,
    Item::Wallpaper,
    Item::Appearance,
    Item::Rule,
    Item::Start("Settings...", launcher::SETTINGS),
    Item::Start("Lock Screen", launcher::LOCK),
    Item::Start("About CorrodeOS", launcher::ABOUT),
];

fn height() -> u32 {
    let rows: i32 = ITEMS
        .iter()
        .map(|item| if matches!(item, Item::Rule) { RULE } else { ROW })
        .sum();
    (2 * PAD + rows) as u32
}

/// Where item `index` starts, down from the top.
fn top_of(index: usize) -> i32 {
    PAD + ITEMS[..index]
        .iter()
        .map(|item| if matches!(item, Item::Rule) { RULE } else { ROW })
        .sum::<i32>()
}

/// The item at `y`, if it is one that can be picked.
fn item_at(x: i32, y: i32) -> Option<usize> {
    if x < 0 || x >= WIDTH as i32 {
        return None;
    }
    (0..ITEMS.len()).find(|index| {
        let top = top_of(*index);
        !matches!(ITEMS[*index], Item::Rule) && y >= top && y < top + ROW
    })
}

fn label(item: Item, theme: &Theme) -> &'static str {
    match item {
        Item::Start(label, _) => label,
        Item::Wallpaper => "Next Wallpaper",
        Item::Appearance if theme.dark => "Light Appearance",
        Item::Appearance => "Dark Appearance",
        Item::Rule => "",
    }
}

fn draw(window: &mut Window, theme: &Theme, lit: Option<usize>) {
    let (text, rule): (Rgb, Rgb) = if theme.dark {
        (rgb(theme.text()), 0xc0ff_ffff)
    } else {
        (rgb(theme.text()), 0xc000_0000)
    };
    window.fill(0, 0, window.width, window.height, CLEAR);
    for (index, item) in ITEMS.iter().enumerate() {
        let top = top_of(index);
        if matches!(item, Item::Rule) {
            window.fill(PAD + 8, top + RULE / 2, WIDTH - 2 * (PAD as u32 + 8), 1, rule);
            continue;
        }
        let color = if lit == Some(index) {
            window.rounded(PAD, top, WIDTH - 2 * PAD as u32, ROW as u32, 6, rgb(theme.accent));
            0xff_ffff
        } else {
            text
        };
        let y = top + (ROW - line_height(TEXT) as i32) / 2;
        window.text(PAD + 10, y, label(*item, theme), TEXT, color);
    }
    window.commit_all();
}

/// Do item `index`, then go.
fn pick(index: usize, theme: &Theme) -> ! {
    let item = ITEMS[index];
    println!("picked {}", label(item, theme));
    match item {
        Item::Start(_, program) => {
            let _ = corrode_rt::call(
                app::LAUNCHER,
                Message::new(launcher::LAUNCH, [program, 0, 0, 0]),
                None,
            );
        }
        Item::Wallpaper => {
            let wallpaper = match theme.wallpaper {
                Wallpaper::Aurora => Wallpaper::Ribbons,
                Wallpaper::Ribbons => Wallpaper::Dunes,
                Wallpaper::Dunes => Wallpaper::Gradient,
                Wallpaper::Gradient => Wallpaper::Aurora,
            };
            let _ = corrode_ui::set_theme(&Theme { wallpaper, ..*theme });
        }
        Item::Appearance => {
            let _ = corrode_ui::set_theme(&Theme {
                dark: !theme.dark,
                ..*theme
            });
        }
        Item::Rule => {}
    }
    corrode_rt::exit();
}

/// The next item that can be picked from `from`, up or down, round the
/// ends.
fn next(from: Option<usize>, down: bool) -> usize {
    let count = ITEMS.len();
    let mut at = from.unwrap_or(if down { count - 1 } else { 0 });
    loop {
        at = if down { (at + 1) % count } else { (at + count - 1) % count };
        if !matches!(ITEMS[at], Item::Rule) {
            return at;
        }
    }
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let mut window = match Window::open_titled(
        WIDTH,
        height(),
        flags::POPOVER | flags::MENU | flags::TRANSLUCENT,
        "Menu",
    ) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut theme = Theme::default();
    let mut lit = None;
    draw(&mut window, &theme, lit);
    println!("open");

    loop {
        match window.wait(u64::MAX) {
            Event::Theme(now) => {
                theme = now;
                draw(&mut window, &theme, lit);
            }
            Event::Hover { x, y } => {
                let now = item_at(x, y);
                if now != lit {
                    lit = now;
                    draw(&mut window, &theme, lit);
                }
            }
            Event::Key {
                key: key @ (KEY_UP | KEY_DOWN),
                pressed: true,
                ..
            } => {
                lit = Some(next(lit, key == KEY_DOWN));
                draw(&mut window, &theme, lit);
            }
            Event::Key {
                key: KEY_ENTER,
                pressed: true,
                ..
            } => {
                if let Some(index) = lit {
                    pick(index, &theme);
                }
            }
            Event::Key {
                key: KEY_ESCAPE,
                pressed: true,
                ..
            } => corrode_rt::exit(),
            Event::Button { x, y, buttons } if buttons & 1 != 0 => {
                if let Some(index) = item_at(x, y) {
                    pick(index, &theme);
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
