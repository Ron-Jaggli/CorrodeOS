//! The panel: a bar along the top of the screen with the system's name, a
//! button that starts a terminal, the app in front -- in bold, as macOS's
//! menu bar has it -- and the date and time, or how long the machine has
//! been up until the desktop says what time it is.
//!
//! It holds one thing the other programs do not: an endpoint to the
//! launcher. That is the whole of its power to start programs, and it can
//! only ask for the ones the launcher knows.
//!
//! The clock is why `SYS_WAIT` takes a deadline. The panel waits for a click
//! or for the next whole second, whichever comes first, and is not
//! scheduled at all in between.
//!
//! It is drawn over the desktop's glass: where it draws nothing, the glass
//! shows. Its colours come from the theme, which it is told when its window
//! opens and again whenever the theme changes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use corrode_rt::abi::{app, launcher, window as flags};
use corrode_rt::{println, Message};
use corrode_ui::{line_height, rgb, width_of, Event, Rgb, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const HEIGHT: u32 = 32;
const NAME: Style = Style::px(16).weight(Weight::Bold);
const LABEL: Style = Style::px(14);

/// The Terminal button.
const BUTTON_AT: (i32, i32, u32, u32) = (132, 5, 92, 22);

/// The colours the theme gives the panel.
struct Look {
    text: Rgb,
    name: Rgb,
    /// A wash over the glass, mostly see-through: the top byte is how
    /// transparent.
    button: Rgb,
    pressed: Rgb,
}

impl Look {
    fn of(theme: &Theme) -> Self {
        let accent = rgb(theme.accent);
        Look {
            text: rgb(theme.text()),
            name: accent,
            button: if theme.dark { 0xd8ff_ffff } else { 0xe000_0000 },
            pressed: 0x6000_0000 | accent,
        }
    }
}

fn button(window: &mut Window, look: &Look, pressed: bool) {
    let (x, y, width, height) = BUTTON_AT;
    // Cleared first: the wash is see-through, so it would otherwise pile up
    // on what was there.
    window.fill(x, y, width, height, CLEAR);
    window.fill(
        x,
        y,
        width,
        height,
        if pressed { look.pressed } else { look.button },
    );
    let text_x = x + (width - width_of("Terminal", LABEL)) as i32 / 2;
    let text_y = y + (height as i32 - line_height(LABEL) as i32) / 2;
    window.text(text_x, text_y, "Terminal", LABEL, look.text);
    window.commit(x, y, width, height);
}

/// What time it is, as the menu bar is told it.
struct Clock {
    /// The wall clock and the monotonic clock at one instant, if the
    /// desktop has said.
    anchor: Option<(i64, u64)>,
    theme: Theme,
}

/// Draw the date and time -- or the uptime, before there is a time to
/// show -- at the right-hand end, and say when it next changes.
fn clock(window: &mut Window, look: &Look, time: &Clock) -> u64 {
    let now = corrode_rt::now_ns();
    let mut buffer = [0u8; 32];
    let (text, next) = match time.anchor {
        Some((unix, at)) => {
            let elapsed = now.saturating_sub(at) / 1_000_000_000;
            let wall = unix + elapsed as i64;
            let text = corrode_ui::theme::clock::format(wall, &time.theme, &mut buffer);
            // The next whole minute, on the monotonic clock.
            let to_minute = (60 - wall.rem_euclid(60)) as u64;
            (format!("{text}"), now + to_minute * 1_000_000_000)
        }
        None => {
            let seconds = now / 1_000_000_000;
            (
                format!(
                    "up {}:{:02}:{:02}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                ),
                (seconds + 1) * 1_000_000_000,
            )
        }
    };
    // Room for the widest the text gets, cleared each time.
    let room = width_of("Wed 30 Sep  12:59 PM", LABEL) + 16;
    let x = window.width as i32 - room as i32 - 8;
    window.fill(x, 0, room, HEIGHT, CLEAR);
    let y = (HEIGHT as i32 - line_height(LABEL) as i32) / 2;
    // Right-aligned, as macOS's clock is.
    let x_text = window.width as i32 - width_of(&text, LABEL) as i32 - 16;
    window.text(x_text, y, &text, LABEL, look.text);
    window.commit(x, 0, room, HEIGHT);
    next
}

/// Where the app in front is named: after the button, up to the middle.
const FRONT_AT: i32 = 240;
const FRONT: Style = Style::px(14).weight(Weight::Bold);

/// Name the app in front, or nobody.
fn front(window: &mut Window, look: &Look, name: &str) {
    let room = window.width / 2 - FRONT_AT as u32;
    window.fill(FRONT_AT, 0, room, HEIGHT, CLEAR);
    let y = (HEIGHT as i32 - line_height(FRONT) as i32) / 2;
    window.text(FRONT_AT, y, name, FRONT, look.text);
    window.commit(FRONT_AT, 0, room, HEIGHT);
}

/// Everything, from nothing.
fn draw(window: &mut Window, look: &Look, name: &str, time: &Clock) -> u64 {
    window.fill(0, 0, window.width, window.height, CLEAR);
    let y = (HEIGHT as i32 - line_height(NAME) as i32) / 2;
    window.text(14, y, "CorrodeOS", NAME, look.name);
    window.commit_all();
    button(window, look, false);
    front(window, look, name);
    clock(window, look, time)
}

fn inside(x: i32, y: i32) -> bool {
    let (left, top, width, height) = BUTTON_AT;
    x >= left && x < left + width as i32 && y >= top && y < top + height as i32
}

fn main() {
    corrode_rt::log::set(app::LOG);
    // Text needs a little memory to draw into; the panel needs no more.
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let mut window = match Window::open_with(0, HEIGHT, flags::DOCKED | flags::TRANSLUCENT) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut look = Look::of(&Theme::default());
    let mut name = alloc::string::String::new();
    let mut time = Clock {
        anchor: None,
        theme: Theme::default(),
    };
    let mut next = draw(&mut window, &look, &name, &time);
    println!("up, {} pixels wide", window.width);

    loop {
        match window.wait(next) {
            Event::Timeout => next = clock(&mut window, &look, &time),
            Event::Theme(theme) => {
                look = Look::of(&theme);
                time.theme = theme;
                next = draw(&mut window, &look, &name, &time);
            }
            Event::Clock { unix, at } => {
                time.anchor = Some((unix, at));
                next = clock(&mut window, &look, &time);
            }
            Event::Active(title) => {
                name = title
                    .iter()
                    .take_while(|byte| **byte != 0)
                    .map(|byte| *byte as char)
                    .collect();
                front(&mut window, &look, &name);
            }
            Event::Button { x, y, buttons } if inside(x, y) => {
                let down = buttons & 1 != 0;
                button(&mut window, &look, down);
                if down {
                    let answer = corrode_rt::call(
                        app::LAUNCHER,
                        Message::new(launcher::LAUNCH, [launcher::TERMINAL, 0, 0, 0]),
                        None,
                    );
                    match answer {
                        Ok(answer) if answer.message.words[0] == 0 => {
                            println!("started a terminal")
                        }
                        Ok(answer) => {
                            println!("the launcher said no ({})", answer.message.words[0])
                        }
                        Err(error) => println!("no launcher: {:?}", error),
                    }
                }
            }
            _ => {}
        }
    }
}
