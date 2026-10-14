//! The panel: a bar along the top of the screen with the system's name, a
//! button that starts a terminal, and how long the machine has been up.
//!
//! It holds one thing the other programs do not: an endpoint to the
//! launcher. That is the whole of its power to start programs, and it can
//! only ask for the ones the launcher knows.
//!
//! The clock is why `SYS_WAIT` takes a deadline. The panel waits for a click
//! or for the next whole second, whichever comes first, and is not
//! scheduled at all in between.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use corrode_rt::abi::{app, launcher};
use corrode_rt::{println, Message};
use corrode_ui::{line_height, width_of, Event, Rgb, Style, Weight, Window};

corrode_rt::entry!(main);

const HEIGHT: u32 = 32;
const NAME: Style = Style::px(16).weight(Weight::Bold);
const LABEL: Style = Style::px(14);

const BACKGROUND: Rgb = 0x001c_1f26;
const NAME_COLOR: Rgb = 0x00e0_7a3c;
const TEXT: Rgb = 0x00e8_e4dc;
const BUTTON: Rgb = 0x003a_3d45;
const BUTTON_PRESSED: Rgb = 0x00b7_410e;

/// The Terminal button.
const BUTTON_AT: (i32, i32, u32, u32) = (132, 5, 92, 22);

fn button(window: &mut Window, pressed: bool) {
    let (x, y, width, height) = BUTTON_AT;
    window.fill(
        x,
        y,
        width,
        height,
        if pressed { BUTTON_PRESSED } else { BUTTON },
    );
    let text_x = x + (width - width_of("Terminal", LABEL)) as i32 / 2;
    let text_y = y + (height as i32 - line_height(LABEL) as i32) / 2;
    window.text(text_x, text_y, "Terminal", LABEL, TEXT);
    window.commit(x, y, width, height);
}

/// Draw the uptime at the right-hand end, and say when it next changes.
fn clock(window: &mut Window) -> u64 {
    let now = corrode_rt::now_ns();
    let seconds = now / 1_000_000_000;
    let text = format!(
        "up {}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    // Room for the widest the text gets, cleared each time.
    let room = width_of("up 00:00:00", LABEL) + 16;
    let x = window.width as i32 - room as i32 - 8;
    window.fill(x, 0, room, HEIGHT, BACKGROUND);
    let y = (HEIGHT as i32 - line_height(LABEL) as i32) / 2;
    window.text(x + 8, y, &text, LABEL, TEXT);
    window.commit(x, 0, room, HEIGHT);
    (seconds + 1) * 1_000_000_000
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
    let mut window = match Window::open(0, HEIGHT, true) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    window.fill(0, 0, window.width, window.height, BACKGROUND);
    let y = (HEIGHT as i32 - line_height(NAME) as i32) / 2;
    window.text(14, y, "CorrodeOS", NAME, NAME_COLOR);
    window.commit_all();
    button(&mut window, false);
    let mut next = clock(&mut window);
    println!("up, {} pixels wide", window.width);

    loop {
        match window.wait(next) {
            Event::Timeout => next = clock(&mut window),
            Event::Button { x, y, buttons } if inside(x, y) => {
                let down = buttons & 1 != 0;
                button(&mut window, down);
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
