//! The lock screen: the whole screen under glass, with the time large in
//! the middle, as macOS's and Plasma's are.
//!
//! It opens a cover window, which the desktop puts above everything and
//! gives every key and click, so nothing behind it can be used. Return or a
//! click on the unlock button takes it away. There are no accounts, so
//! there is no password -- this keeps the screen out of view, not safe.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use corrode_rt::abi::{app, window as flags};
use corrode_rt::println;
use corrode_ui::{line_height, rgb, width_of, Event, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const TIME: Style = Style::px(112).weight(Weight::Bold);
const DATE: Style = Style::px(24).weight(Weight::Bold);
const NAME: Style = Style::px(16).weight(Weight::Bold);
const HINT: Style = Style::px(13);
const AVATAR: u32 = 64;
const BUTTON: (u32, u32) = (220, 32);
/// Return, by scancode.
const KEY_ENTER: u8 = 0x1c;

struct Clock {
    anchor: Option<(i64, u64)>,
    theme: Theme,
}

/// The date and the time, apart; and when the minute next turns.
fn now(clock: &Clock) -> (alloc::string::String, alloc::string::String, u64) {
    let now = corrode_rt::now_ns();
    match clock.anchor {
        Some((unix, at)) => {
            let wall = unix + (now.saturating_sub(at) / 1_000_000_000) as i64;
            let mut buffer = [0u8; 32];
            let text = corrode_ui::theme::clock::format(wall, &clock.theme, &mut buffer);
            // "Thu 19 Nov  14:05": the date, two spaces, the time.
            let (date, time) = text.split_once("  ").unwrap_or(("", text));
            let to_minute = (60 - wall.rem_euclid(60)) as u64;
            (format!("{date}"), format!("{time}"), now + to_minute * 1_000_000_000)
        }
        None => {
            let seconds = now / 1_000_000_000;
            let time = format!("{}:{:02}", seconds / 3600, seconds / 60 % 60);
            (format!("Up for"), time, (seconds / 60 + 1) * 60 * 1_000_000_000)
        }
    }
}

/// Where the unlock button is.
fn button(window: &Window) -> (i32, i32) {
    (
        (window.width - BUTTON.0) as i32 / 2,
        window.height as i32 - 120,
    )
}

fn draw(window: &mut Window, clock: &Clock) -> u64 {
    let theme = &clock.theme;
    let (width, height) = (window.width, window.height);
    // Clear: the desktop's glass, the whole screen of it, is the backdrop.
    window.fill(0, 0, width, height, CLEAR);
    let text = rgb(theme.text());
    let soft = if theme.dark { 0xb0ff_ffff } else { 0xc000_0000 };
    let centre = |window: &mut Window, y: i32, line: &str, style: Style, color| {
        let x = (width as i32 - width_of(line, style) as i32) / 2;
        window.text(x, y, line, style, color);
    };

    let (date, time, next) = now(clock);
    let top = height as i32 / 6;
    centre(window, top, &date, DATE, text);
    centre(window, top + line_height(DATE) as i32, &time, TIME, text);

    // Who is signed in -- there is only ever the one.
    let (bx, by) = button(window);
    let avatar_y = by - 40 - line_height(NAME) as i32 - AVATAR as i32;
    let ax = (width - AVATAR) as i32 / 2;
    window.rounded(ax, avatar_y, AVATAR, AVATAR, AVATAR / 2, rgb(theme.accent));
    let initial = Style::px(30).weight(Weight::Bold);
    let iy = avatar_y + (AVATAR as i32 - line_height(initial) as i32) / 2;
    centre(window, iy, "C", initial, 0xff_ffff);
    centre(window, avatar_y + AVATAR as i32 + 10, "CorrodeOS", NAME, text);

    window.rounded(bx, by, BUTTON.0, BUTTON.1, BUTTON.1 / 2, soft);
    let hy = by + (BUTTON.1 as i32 - line_height(HINT) as i32) / 2;
    centre(window, hy, "Press Return to unlock", HINT, text);
    window.commit_all();
    next
}

fn unlock() -> ! {
    println!("unlocked");
    corrode_rt::exit();
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let mut window = match Window::open_titled(0, 0, flags::COVER | flags::TRANSLUCENT, "Lock") {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut clock = Clock {
        anchor: None,
        theme: Theme::default(),
    };
    let mut next = draw(&mut window, &clock);
    println!("locked, {}x{}", window.width, window.height);

    loop {
        match window.wait(next) {
            Event::Timeout => next = draw(&mut window, &clock),
            Event::Theme(theme) => {
                clock.theme = theme;
                next = draw(&mut window, &clock);
            }
            Event::Clock { unix, at } => {
                clock.anchor = Some((unix, at));
                next = draw(&mut window, &clock);
            }
            Event::Key {
                key: KEY_ENTER,
                pressed: true,
                ..
            } => unlock(),
            Event::Button { x, y, buttons } if buttons & 1 != 0 => {
                let (bx, by) = button(&window);
                if x >= bx && x < bx + BUTTON.0 as i32 && y >= by && y < by + BUTTON.1 as i32 {
                    unlock();
                }
            }
            _ => {}
        }
    }
}
