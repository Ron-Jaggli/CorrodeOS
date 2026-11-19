//! A terminal: a window of text you can type commands into.
//!
//! The first program that is an application rather than a driver or a
//! check. It holds what every desktop program is given -- an endpoint to
//! the window server, a pool of memory, the log -- and from those makes
//! itself a heap and a window, then waits for keys.
//!
//! Lines go to `corrode-shell`, which has the commands about text and files
//! and runs them in pipes and lists. The files are the ones packed from
//! `root/` at build time, handed over as a read-only blob; a terminal started
//! without them has an empty tree. What only a running program can answer --
//! the clock, its memory, the capabilities it holds -- is this terminal's,
//! and the shell runs those commands like its own.
//!
//! Where a Linux command means the same thing here it has the same name, so
//! fingers that know one know the other. The prompt is `corrode-line`: arrow
//! keys, history, Tab to complete a command, and the usual Ctrl chords.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use corrode_input::Key;
use corrode_line::{Editor, Outcome};
use corrode_rt::abi::{app, kind, rights, window};
use corrode_rt::println;
use corrode_shell::{About, Fs, Host, Output, Shell};
use corrode_ui::{line_height, width_of, Event, Rgb, Style, Window};

corrode_rt::entry!(main);

const WIDTH: u32 = 600;
const HEIGHT: u32 = 340;
const MARGIN: i32 = 10;
const STYLE: Style = Style::px(16);
const PROMPT_TEXT: &str = "> ";
/// The cursor: a bar this wide, before the character it is on.
const CARET: u32 = 2;

const BACKGROUND: Rgb = 0x0015_1820;
const TEXT: Rgb = 0x00e8_e4dc;
const PROMPT: Rgb = 0x00e0_7a3c;
const QUIET: Rgb = 0x008a_909c;
const ERROR: Rgb = 0x00e8_6a5c;

/// Pages of heap to start with: a megabyte.
const HEAP_PAGES: usize = 256;
/// Lines kept for scrolling back. Older ones are dropped.
const KEEP: usize = 500;
/// The longest `sleep`, so a slip of the finger can't lose the terminal.
const SLEEP_MAX_NS: u64 = 3600 * 1_000_000_000;

/// Slots `caps` looks in. Every desktop program's are below this.
const SLOTS: usize = 16;

/// The terminal's own commands, how to call them, and what they do. The
/// shell lists them in `help` after its own.
const COMMANDS: [About; 9] = [
    ("clear", "", "start with an empty window (or Ctrl+L)"),
    ("history", "", "the lines typed so far"),
    ("uptime", "", "time since the machine started"),
    ("free", "", "this program's memory"),
    ("uname", "[-asrm]", "what system this is"),
    ("sleep", "SECONDS", "wait; Ctrl+C to stop early"),
    ("caps", "", "the capabilities this terminal holds"),
    ("about", "", "what this is"),
    ("exit", "", "close this terminal"),
];

struct Terminal {
    window: Window,
    /// Finished lines, already wrapped to the window, with their colour.
    lines: Vec<(String, Rgb)>,
    editor: Editor,
    focused: bool,
    line_height: u32,
    /// While a command is running, when it finishes. Lines entered meanwhile
    /// wait in `queued`, as a shell's typeahead does.
    busy_until: Option<u64>,
    queued: Vec<String>,
    shell: Shell,
    fs: Fs<'static>,
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
        // A command is running: no prompt until it is done.
        if self.busy_until.is_some() {
            self.window.commit_all();
            return;
        }
        let x = MARGIN + self.window.text(MARGIN, y, PROMPT_TEXT, STYLE, PROMPT) as i32;

        // A line wider than the window scrolls sideways to keep the cursor
        // in view: drop characters from the front until it fits.
        let room = (self.width() - width_of(PROMPT_TEXT, STYLE)).saturating_sub(CARET);
        let (line, cursor) = (self.editor.line(), self.editor.cursor());
        let mut start = 0;
        while start < cursor && width_of(&line[start..cursor], STYLE) > room {
            start += 1;
        }
        let caret_x = x + width_of(&line[start..cursor], STYLE) as i32;
        let visible = String::from(&line[start..]);
        self.window.text(x, y, &visible, STYLE, TEXT);
        if self.focused {
            let bar = self.line_height.saturating_sub(4);
            self.window.fill(caret_x, y + 1, CARET, bar, PROMPT);
        }
        self.window.commit_all();
    }

    fn key(&mut self, key: Key, text: Option<char>, modifiers: u8) {
        match self.editor.key(key, text, modifiers) {
            Outcome::Submit(line) if self.busy_until.is_some() => self.queued.push(line),
            Outcome::Submit(line) => self.run(&line),
            Outcome::Cancel(line) => {
                if self.busy_until.take().is_some() {
                    self.queued.clear();
                }
                self.say(&format!("{PROMPT_TEXT}{line}^C"), QUIET);
            }
            Outcome::ClearScreen => self.lines.clear(),
            Outcome::Complete => {
                let names = self.shell.names(&System::new(&[]));
                let choices = self.editor.complete(&names);
                if !choices.is_empty() {
                    self.say(&choices.join("  "), QUIET);
                }
            }
            Outcome::Edited | Outcome::Ignored => {}
        }
    }

    /// The running command's time is up: carry on with what was typed
    /// meanwhile.
    fn finish(&mut self) {
        self.busy_until = None;
        while self.busy_until.is_none() && !self.queued.is_empty() {
            let line = self.queued.remove(0);
            self.run(&line);
        }
    }

    fn run(&mut self, line: &str) {
        println!("$ {}", line);
        self.say(&format!("{PROMPT_TEXT}{line}"), QUIET);
        let mut system = System::new(self.editor.history());
        let printed = self.shell.run(line, &self.fs, &mut system);
        let System {
            clear, sleep, exit, ..
        } = system;
        if exit {
            close();
        }
        if clear {
            self.lines.clear();
        }
        for line in printed {
            self.say(&line.text, if line.error { ERROR } else { TEXT });
        }
        // The shell has run the whole line by now, so a sleep holds the
        // prompt after it rather than between its commands.
        if let Some(ns) = sleep {
            self.busy_until = Some(corrode_rt::now_ns().saturating_add(ns));
        }
    }
}

/// The commands only a running terminal can answer, for the shell to run.
/// What they ask of the terminal itself -- an empty window, a wait -- is left
/// here for it to do once the line has run.
struct System<'a> {
    history: &'a [String],
    clear: bool,
    sleep: Option<u64>,
    exit: bool,
}

impl<'a> System<'a> {
    fn new(history: &'a [String]) -> Self {
        System {
            history,
            clear: false,
            sleep: None,
            exit: false,
        }
    }
}

impl Host for System<'_> {
    fn commands(&self) -> &[About] {
        &COMMANDS
    }

    fn run(&mut self, args: &[String], _input: &str, out: &mut Output) -> Option<i32> {
        let rest = &args[1..];
        Some(match args[0].as_str() {
            "clear" => {
                self.clear = true;
                0
            }
            "history" => {
                for (number, line) in self.history.iter().enumerate() {
                    out.line(&format!("{:>5}  {line}", number + 1));
                }
                0
            }
            "uptime" => {
                let seconds = corrode_rt::now_ns() / 1_000_000_000;
                out.line(&format!(
                    "up {}:{:02}:{:02}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                ));
                0
            }
            "free" => {
                let (free, total) = corrode_rt::heap::stats();
                // The typeface is proportional, so no columns: a sentence.
                out.line(&format!(
                    "heap: {} KiB total, {} KiB used, {} KiB free",
                    total / 1024,
                    (total - free) / 1024,
                    free / 1024
                ));
                out.line("this program's own pool; there is no system-wide view yet");
                0
            }
            "uname" => match uname(rest.iter().map(String::as_str)) {
                Ok(text) => {
                    out.line(&text);
                    0
                }
                Err(flag) => {
                    out.error(&format!("uname: unknown option {flag}"));
                    1
                }
            },
            "sleep" => match rest.first().and_then(|text| seconds_to_ns(text)) {
                Some(ns) => {
                    self.sleep = Some(ns);
                    0
                }
                None => {
                    out.error("sleep: give a number of seconds, like 2 or 0.5");
                    1
                }
            },
            "exit" => {
                self.exit = true;
                0
            }
            "caps" => {
                caps(out);
                0
            }
            "about" => {
                out.line("CorrodeOS terminal: a ring 3 program holding a window,");
                out.line("a pool of memory, the log and the boot medium's files,");
                out.line("and nothing else. caps lists them.");
                0
            }
            _ => return None,
        })
    }
}

/// Go, and take the window with us: the window manager drops a window
/// whose program has finished.
fn close() -> ! {
    println!("closed");
    corrode_rt::exit();
}

/// Every capability this terminal holds, a slot to a line.
fn caps(out: &mut Output) {
    for slot in 0..SLOTS {
        let Ok((what, held)) = corrode_rt::identify(slot) else {
            continue;
        };
        let name = match what {
            kind::CONSOLE => "console",
            kind::FRAME => "frame",
            kind::TASK => "task",
            kind::ENDPOINT => "endpoint",
            kind::REPLY => "reply",
            kind::SURFACE => "surface",
            kind::PCI_DEVICE => "pci device",
            kind::DEVICE_MEMORY => "device memory",
            kind::INTERRUPT => "interrupt",
            kind::MEMORY => "memory",
            kind::BLOB => "blob",
            kind::IO_PORTS => "i/o ports",
            kind::POOL => "pool",
            kind::WINDOW => "window",
            _ => "unknown",
        };
        let mut letters = String::new();
        for (bit, letter) in [
            (rights::READ, 'r'),
            (rights::WRITE, 'w'),
            (rights::GRANT, 'g'),
        ] {
            letters.push(if held & bit != 0 { letter } else { '-' });
        }
        let what = match slot {
            app::WINDOWS => "the window server",
            app::POOL => "memory to grow into",
            app::LOG => "the log",
            app::WINDOW => "this window",
            app::HEAP => "the heap",
            app::FILES => "the files",
            _ => "",
        };
        out.line(format!("{slot:>3}  {letters}  {name}  {what}").trim_end());
    }
}

/// The files from the boot medium, if this terminal was given them.
fn files() -> Fs<'static> {
    if corrode_rt::identify(app::FILES).map(|(what, _)| what) != Ok(kind::BLOB) {
        println!("no files were given");
        return Fs::empty();
    }
    let Ok(mapping) = corrode_rt::map(app::FILES) else {
        println!("could not map the files");
        return Fs::empty();
    };
    // SAFETY: the kernel mapped `len` bytes here read-only for the life of
    // this task, and no holder of a blob can write it.
    let bytes = unsafe { core::slice::from_raw_parts(mapping.address, mapping.len) };
    match Fs::from_tar(bytes) {
        Ok(fs) => {
            println!("{} files and directories", fs.len());
            fs
        }
        Err(error) => {
            println!("the files are not a tar archive: {:?}", error);
            Fs::empty()
        }
    }
}

/// What `uname` prints for its arguments, or the first one it doesn't know.
fn uname<'a>(args: impl Iterator<Item = &'a str>) -> Result<String, &'a str> {
    let (mut name, mut release, mut machine) = (false, false, false);
    for arg in args {
        let flags = arg.strip_prefix('-').filter(|f| !f.is_empty()).ok_or(arg)?;
        for flag in flags.chars() {
            match flag {
                's' => name = true,
                'r' => release = true,
                'm' => machine = true,
                'a' => (name, release, machine) = (true, true, true),
                _ => return Err(arg),
            }
        }
    }
    if !(release || machine) {
        name = true;
    }
    let parts = [
        (name, "Corrode"),
        (release, env!("CARGO_PKG_VERSION")),
        (machine, "x86_64"),
    ];
    let shown: Vec<&str> = parts
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, s)| *s)
        .collect();
    Ok(shown.join(" "))
}

/// `2`, `0.5` or `1.25` seconds, in nanoseconds, up to `SLEEP_MAX_NS`.
fn seconds_to_ns(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() && fraction.is_empty() || fraction.len() > 9 {
        return None;
    }
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(fraction) {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut nanos: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    for _ in fraction.len()..9 {
        nanos *= 10;
    }
    let ns = whole.checked_mul(1_000_000_000)?.checked_add(nanos)?;
    (ns <= SLEEP_MAX_NS).then_some(ns)
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, HEAP_PAGES).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let window = match Window::open_titled(WIDTH, HEIGHT, window::RESIZABLE, "Terminal") {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let mut terminal = Terminal {
        window,
        lines: Vec::new(),
        editor: Editor::new(),
        focused: true,
        line_height: line_height(STYLE) + 2,
        busy_until: None,
        queued: Vec::new(),
        shell: Shell::new(),
        fs: files(),
    };
    terminal.say("CorrodeOS terminal", PROMPT);
    let motd = terminal.fs.get("/etc/motd").map(|motd| motd.data);
    match motd.map(String::from_utf8_lossy) {
        Some(text) => {
            for line in text.lines() {
                terminal.say(line, QUIET);
            }
        }
        None => terminal.say("type help and press Enter", QUIET),
    }
    terminal.draw();
    println!("open, {}x{}", terminal.window.width, terminal.window.height);

    loop {
        match terminal
            .window
            .wait(terminal.busy_until.unwrap_or(u64::MAX))
        {
            Event::Key {
                pressed: true,
                key,
                text,
                modifiers,
            } => terminal.key(Key(key), text, modifiers),
            Event::Focus(focused) => terminal.focused = focused,
            Event::Close => close(),
            // Everything is laid out from the window's size each time.
            Event::Resize { width, height } => println!("resized to {}x{}", width, height),
            Event::Timeout => match terminal.busy_until {
                Some(until) if corrode_rt::now_ns() >= until => terminal.finish(),
                _ => continue,
            },
            _ => continue,
        }
        terminal.draw();
    }
}
