//! About: what this system is, macOS's "About This Mac" for CorrodeOS.
//!
//! A logo, the name, and a few lines of what the machine is running, over
//! the window's glass. It asks the kernel nothing it could not already: the
//! clock is the only thing it reads.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;

use corrode_rt::abi::{app, window as flags};
use corrode_rt::println;
use corrode_ui::{line_height, rgb, width_of, Event, Rgb, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const WIDTH: u32 = 360;
const HEIGHT: u32 = 324;

const NAME: Style = Style::px(26).weight(Weight::Bold);
const LINE: Style = Style::px(13);
const KEY: Style = Style::px(13).weight(Weight::Bold);

/// What is said about the system, a key and its value per line.
const FACTS: [(&str, &str); 4] = [
    ("Kernel", "capability microkernel, x86_64"),
    ("Written in", "Rust, no_std, no crates.io"),
    ("Desktop", "liquid glass, software rendered"),
    ("Glass", "ported from Glass-HQ, MIT"),
];

fn draw(window: &mut Window, theme: &Theme) {
    let dark = theme.dark;
    let text = rgb(theme.text());
    let secondary: Rgb = if dark { 0x9898_9f } else { 0x6c6c_70 };
    let accent = rgb(theme.accent);
    window.fill(0, 0, window.width, window.height, CLEAR);
    window.fill(
        0,
        0,
        window.width,
        window.height,
        if dark { 0x2018_181a } else { 0x20f2_f2f7 },
    );

    // The logo: a rounded tile in the accent with a white "C" over it.
    let (logo, top) = (72u32, 22i32);
    let left = (WIDTH - logo) as i32 / 2;
    window.rounded(left, top, logo, logo, 18, accent);
    let mark = Style::px(46).weight(Weight::Bold);
    let mark_width = width_of("C", mark);
    window.text(
        left + (logo - mark_width) as i32 / 2,
        top + (logo as i32 - line_height(mark) as i32) / 2,
        "C",
        mark,
        0xff_ffff,
    );

    let mut y = top + logo as i32 + 12;
    let name = "CorrodeOS";
    window.text((WIDTH - width_of(name, NAME)) as i32 / 2, y, name, NAME, text);
    y += line_height(NAME) as i32;
    let version = concat!("Version ", env!("CARGO_PKG_VERSION"), " - desktop-glass");
    window.text(
        (WIDTH - width_of(version, LINE)) as i32 / 2,
        y,
        version,
        LINE,
        secondary,
    );
    y += line_height(LINE) as i32 + 14;

    // Keys right-aligned to a column, values left of it, as macOS does.
    let column = 128;
    for (key, value) in FACTS {
        window.text(column - width_of(key, KEY) as i32, y, key, KEY, text);
        window.text(column + 10, y, value, LINE, secondary);
        y += line_height(LINE) as i32 + 4;
    }
    window.commit_all();
}

/// The uptime line at the bottom, redrawn each second. When it next changes.
fn uptime(window: &mut Window, theme: &Theme) -> u64 {
    let seconds = corrode_rt::now_ns() / 1_000_000_000;
    let line = format!(
        "Up {}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    let y = HEIGHT as i32 - line_height(LINE) as i32 - 14;
    let height = line_height(LINE);
    window.fill(0, y, WIDTH, height, CLEAR);
    window.fill(
        0,
        y,
        WIDTH,
        height,
        if theme.dark { 0x2018_181a } else { 0x20f2_f2f7 },
    );
    let color = if theme.dark { 0x9898_9f } else { 0x6c6c_70 };
    window.text((WIDTH - width_of(&line, LINE)) as i32 / 2, y, &line, LINE, color);
    window.commit(0, y, WIDTH, height);
    (seconds + 1) * 1_000_000_000
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, 16).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let mut window = match Window::open_titled(WIDTH, HEIGHT, flags::TRANSLUCENT, "About") {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut theme = Theme::default();
    draw(&mut window, &theme);
    let mut next = uptime(&mut window, &theme);
    println!("open");

    loop {
        match window.wait(next) {
            Event::Timeout => next = uptime(&mut window, &theme),
            Event::Theme(now) => {
                theme = now;
                draw(&mut window, &theme);
                next = uptime(&mut window, &theme);
            }
            Event::Close => {
                println!("closed");
                corrode_rt::exit();
            }
            _ => {}
        }
    }
}
