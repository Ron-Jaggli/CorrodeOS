//! A terminal: a window of text you can type commands into.
//!
//! The first program that is an application rather than a driver or a
//! check. It holds what every desktop program is given -- an endpoint to
//! the window server, a pool of memory, the log -- and from those makes
//! itself a heap and a window, then waits for keys.
//!
//! There is no shell and nothing to run yet, so the commands are built in.
//! What it shows is the machinery: lines that wrap and scroll, a prompt that
//! knows whether it has the keyboard, memory it bought for itself.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use corrode_rt::abi::app;
use corrode_rt::println;
use corrode_ui::{line_height, width_of, Event, Rgb, Style, Window};

corrode_rt::entry!(main);

const WIDTH: u32 = 600;
const HEIGHT: u32 = 340;
const MARGIN: i32 = 10;
const STYLE: Style = Style::px(16);

const BACKGROUND: Rgb = 0x0015_1820;
const TEXT: Rgb = 0x00e8_e4dc;
const PROMPT: Rgb = 0x00e0_7a3c;
const QUIET: Rgb = 0x008a_909c;

/// Pages of heap to start with: a megabyte.
const HEAP_PAGES: usize = 256;
/// Lines kept for scrolling back. Older ones are dropped.
const KEEP: usize = 500;
const INPUT_MAX: usize = 120;

struct Terminal {
    window: Window,
    /// Finished lines, already wrapped to the window, with their colour.
    lines: Vec<(String, Rgb)>,
    input: String,
    focused: bool,
    line_height: u32,
}

impl Terminal {
    fn width(&self) -> u32 {
        self.window.width - 2 * MARGIN as u32
    }

    /// Add a line of output, wrapped to the window.
    fn say(&mut self, text: &str, color: Rgb) {
        let width = self.width();
        let mut line = String::new();
        for c in text.chars() {
            line.push(c);
            if width_of(&line, STYLE) > width {
                line.pop();
                self.lines.push((core::mem::take(&mut line), color));
                line.push(c);
            }
        }
        self.lines.push((line, color));
        if self.lines.len() > KEEP {
            self.lines.drain(..self.lines.len() - KEEP);
        }
    }

    fn draw(&mut self) {
        let (width, height) = (self.window.width, self.window.height);
        self.window.fill(0, 0, width, height, BACKGROUND);
        let rows = ((height as i32 - 2 * MARGIN) / self.line_height as i32).max(1) as usize;

        // The prompt is the last row; the newest lines fill the rest.
        let shown = rows - 1;
        let first = self.lines.len().saturating_sub(shown);
        let mut y = MARGIN;
        for index in first..self.lines.len() {
            let (text, color) = &self.lines[index];
            self.window.text(MARGIN, y, text, STYLE, *color);
            y += self.line_height as i32;
        }
        let x = MARGIN + self.window.text(MARGIN, y, "> ", STYLE, PROMPT) as i32;
        let caret = if self.focused { "_" } else { "" };
        let input = format!("{}{}", self.input, caret);
        self.window.text(x, y, &input, STYLE, TEXT);
        self.window.commit_all();
    }

    fn run(&mut self, line: &str) {
        println!("$ {}", line);
        self.say(&format!("> {line}"), QUIET);
        let mut words = line.split_whitespace();
        let Some(command) = words.next() else {
            return;
        };
        let rest = line[command.len()..].trim_start();
        match command {
            "help" => {
                for (name, what) in [
                    ("help", "this list"),
                    ("echo TEXT", "say TEXT back"),
                    ("clear", "start with an empty window"),
                    ("uptime", "time since the machine started"),
                    ("mem", "this program's heap"),
                    ("about", "what this is"),
                ] {
                    self.say(&format!("  {name:<10} {what}"), TEXT);
                }
            }
            "echo" => self.say(rest, TEXT),
            "clear" => self.lines.clear(),
            "uptime" => {
                let seconds = corrode_rt::now_ns() / 1_000_000_000;
                self.say(
                    &format!(
                        "up {}:{:02}:{:02}",
                        seconds / 3600,
                        seconds / 60 % 60,
                        seconds % 60
                    ),
                    TEXT,
                );
            }
            "mem" => {
                let (free, total) = corrode_rt::heap::stats();
                self.say(
                    &format!(
                        "heap: {} KiB of {} KiB free, from this program's own pool",
                        free / 1024,
                        total / 1024
                    ),
                    TEXT,
                );
            }
            "about" => {
                self.say(
                    "CorrodeOS terminal: a ring 3 program holding a window,",
                    TEXT,
                );
                self.say("a pool of memory and the log, and nothing else.", TEXT);
            }
            _ => self.say(&format!("unknown command: {command} (try help)"), TEXT),
        }
    }

    fn key(&mut self, text: char) {
        match text {
            '\u{8}' => {
                self.input.pop();
            }
            '\n' => {
                let line = core::mem::take(&mut self.input);
                self.run(&line);
            }
            ' '..='~' if self.input.len() < INPUT_MAX => self.input.push(text),
            _ => {}
        }
    }
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, HEAP_PAGES).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let window = match Window::open(WIDTH, HEIGHT, false) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut terminal = Terminal {
        window,
        lines: Vec::new(),
        input: String::new(),
        focused: true,
        line_height: line_height(STYLE) + 2,
    };
    terminal.say("CorrodeOS terminal", PROMPT);
    terminal.say("type help and press Enter", QUIET);
    terminal.draw();
    println!("open, {}x{}", terminal.window.width, terminal.window.height);

    loop {
        match terminal.window.wait(u64::MAX) {
            Event::Key {
                pressed: true,
                text: Some(text),
                ..
            } => terminal.key(text),
            Event::Focus(focused) => terminal.focused = focused,
            _ => continue,
        }
        terminal.draw();
    }
}
