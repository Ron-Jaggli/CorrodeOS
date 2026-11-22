//! Files: the root filesystem, as Finder shows a disk.
//!
//! A sidebar of places -- the root and each directory in it -- a list of
//! what is in the current directory, and a preview of the file selected in
//! it: its text, if it is text. Folders open with a click, or with Return;
//! the arrows move the selection and Backspace goes up a level.
//!
//! A Windows program (`.exe`) opens the same way, and runs: the desktop
//! starts it seeing its own folder alone, as the terminal's `win` does
//! (`corrode_rt::win`), and the preview shows what it wrote.
//!
//! It reads the same archive the terminal does, through the same read-only
//! capability, and can no more change it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use corrode_rt::abi::{app, kind, window as flags};
use corrode_rt::println;
use corrode_shell::Fs;
use corrode_ui::{line_height, rgb, width_of, Event, Rgb, Style, Theme, Weight, Window, CLEAR};

corrode_rt::entry!(main);

const WIDTH: u32 = 700;
const HEIGHT: u32 = 420;
const HEAP_PAGES: usize = 192;

const SIDEBAR: i32 = 160;
const TOOLBAR: i32 = 44;
const ROW: i32 = 26;

const TITLE: Style = Style::px(15).weight(Weight::Bold);
const LABEL: Style = Style::px(13);
const SMALL: Style = Style::px(11).weight(Weight::Bold);

/// Scancodes, set 1.
const KEY_UP: u8 = 0x80 | 0x48;
const KEY_DOWN: u8 = 0x80 | 0x50;
const KEY_ENTER: u8 = 0x1c;
const KEY_BACKSPACE: u8 = 0x0e;

/// macOS's folder blue.
const FOLDER: Rgb = 0x1e_9bf0;

struct Look {
    background: Rgb,
    sidebar: Rgb,
    text: Rgb,
    secondary: Rgb,
    separator: Rgb,
    selected: Rgb,
    paper: Rgb,
}

impl Look {
    fn of(theme: &Theme) -> Self {
        let dark = theme.dark;
        Look {
            background: if dark { 0x2018_181a } else { 0x20f6_f6f8 },
            sidebar: if dark { 0x6010_1012 } else { 0x60e4_e4ea },
            text: rgb(theme.text()),
            secondary: if dark { 0x9898_9f } else { 0x6c6c_70 },
            separator: if dark { 0x3a3a_3c } else { 0xd8d8_dc },
            selected: 0x4000_0000 | rgb(theme.accent),
            paper: if dark { 0xe8e8_ec } else { 0xffff_ff },
        }
    }
}

struct Browser {
    window: Window,
    fs: Fs<'static>,
    theme: Theme,
    /// The directory shown, absolute.
    cwd: String,
    /// What is in it: folders first, then files, each by name.
    entries: Vec<String>,
    /// Which of `entries` is selected.
    selected: Option<usize>,
    /// The first of `entries` the list shows, scrolled.
    top: usize,
    /// The first line of the selected file the preview shows, scrolled.
    preview_top: usize,
    /// The sidebar's places: the root, then each directory in it.
    places: Vec<String>,
    /// The Windows program last run, for the preview while it is selected.
    ran: Option<Ran>,
}

/// A Windows program Files ran: what it wrote, and how it ended.
struct Ran {
    path: String,
    output: String,
    note: String,
}

/// Lines of a program's output kept for the preview.
const OUTPUT_LINES: usize = 500;

/// Whether `path` names a Windows program, as Explorer would run one.
fn is_program(path: &str) -> bool {
    let name = name_of(path).as_bytes();
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".exe")
}

/// "/etc/motd" -> "motd"; "/" -> "/".
fn name_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((_, "")) | None => path,
        Some((_, name)) => name,
    }
}

fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => String::from("/"),
        Some((parent, _)) => String::from(parent),
    }
}

/// A size as Finder writes it.
fn size(bytes: usize) -> String {
    match bytes {
        0..=999 => format!("{bytes} bytes"),
        1000..=999_999 => format!("{}.{} KB", bytes / 1000, bytes / 100 % 10),
        _ => format!("{}.{} MB", bytes / 1_000_000, bytes / 100_000 % 10),
    }
}

/// As much of `text` as fits in `width` pixels, with an ellipsis if cut.
fn fit(text: &str, style: Style, width: u32) -> String {
    if width_of(text, style) <= width {
        return String::from(text);
    }
    let mut out = String::new();
    for c in text.chars() {
        out.push(c);
        if width_of(&out, style) + width_of("...", style) > width {
            out.pop();
            break;
        }
    }
    out.push_str("...");
    out
}

impl Browser {
    fn open(&mut self, dir: &str) {
        if self.cwd != dir {
            // The title bar names the folder, as Finder's does.
            corrode_ui::subtitle(if dir == "/" { "CorrodeOS" } else { name_of(dir) });
        }
        self.cwd = String::from(dir);
        let mut folders = Vec::new();
        let mut files = Vec::new();
        for node in self.fs.children(dir) {
            if node.is_dir() {
                folders.push(node.path.clone());
            } else {
                files.push(node.path.clone());
            }
        }
        folders.sort();
        files.sort();
        folders.extend(files);
        self.entries = folders;
        self.selected = None;
        self.top = 0;
        self.preview_top = 0;
        println!("{} ({} items)", self.cwd, self.entries.len());
    }

    fn is_dir(&self, path: &str) -> bool {
        self.fs.get(path).is_some_and(|node| node.is_dir())
    }

    /// Open the selected entry: into a folder, run a Windows program, or
    /// nothing for another file, whose preview is already showing.
    fn activate(&mut self) {
        let Some(path) = self.selected.and_then(|index| self.entries.get(index)).cloned() else {
            return;
        };
        if self.is_dir(&path) {
            self.open(&path);
        } else if is_program(&path) {
            self.run(&path);
        }
    }

    /// Run the Windows program at `path` and keep what it writes. Files
    /// waits while it runs -- the kernel can only block on its output --
    /// which is at most 10 seconds: one still running then is stopped.
    fn run(&mut self, path: &str) {
        self.ran = Some(Ran {
            path: String::from(path),
            output: String::new(),
            note: String::from("Running..."),
        });
        self.draw();
        let name = name_of(path);
        let mut output = String::new();
        let mut line = String::new();
        let mut lines = 0;
        let ended = corrode_rt::win::run(&format!("{path}\n"), |piece, ends| {
            line.push_str(piece);
            if ends {
                println!("{name}: {line}");
                if lines < OUTPUT_LINES {
                    output.push_str(&line);
                    output.push('\n');
                }
                line.clear();
                lines += 1;
            }
        });
        output.push_str(&line);
        use corrode_rt::win::Ended;
        let note = match ended {
            Ok(Ended::Ended) => format!("Ended after {lines} lines"),
            Ok(Ended::Stopped) => String::from("Ran 10 seconds and was stopped"),
            Ok(Ended::StillRunning) => String::from("Still running; no more is shown"),
            Ok(Ended::Lost) => String::from("Its output was lost"),
            Err(error) => format!("Not run: {}", error.why()),
        };
        println!("{name}: {}", note.to_ascii_lowercase());
        self.ran = Some(Ran {
            path: String::from(path),
            output,
            note,
        });
    }

    fn list_width(&self) -> i32 {
        (self.window.width as i32 - SIDEBAR) * 55 / 100
    }

    fn draw(&mut self) {
        let look = Look::of(&self.theme);
        let (width, height) = (self.window.width, self.window.height);
        let list = self.list_width();
        let rows = self.rows();
        let window = &mut self.window;
        window.fill(0, 0, width, height, CLEAR);
        window.fill(0, 0, width, height, look.background);

        // The sidebar.
        window.fill(0, 0, SIDEBAR as u32, height, look.sidebar);
        window.text(14, 14, "Places", SMALL, look.secondary);
        for (index, place) in self.places.iter().enumerate() {
            let y = 36 + index as i32 * ROW;
            if *place == self.cwd {
                window.rounded(8, y, SIDEBAR as u32 - 16, ROW as u32 - 2, 6, look.selected);
            }
            folder(window, 16, y + 5, 16);
            let name = if place == "/" { "CorrodeOS" } else { name_of(place) };
            let label = fit(name, LABEL, SIDEBAR as u32 - 56);
            window.text(40, y + (ROW - line_height(LABEL) as i32) / 2 - 1, &label, LABEL, look.text);
        }

        // The toolbar: back, and where this is.
        let left = SIDEBAR;
        window.fill(left, TOOLBAR - 1, width - left as u32, 1, look.separator);
        let back = if self.cwd == "/" { look.secondary } else { look.text };
        window.rounded(left + 10, 10, 26, 24, 6, look.selected & 0x00ff_ffff | 0xc000_0000);
        window.text(left + 18, 12, "<", TITLE, back);
        let here = if self.cwd == "/" { "CorrodeOS" } else { name_of(&self.cwd) };
        window.text(left + 48, 13, here, TITLE, look.text);

        // The list.
        let top = self.top;
        for (index, path) in self.entries.iter().enumerate().skip(top).take(rows) {
            let y = TOOLBAR + 4 + (index - top) as i32 * ROW;
            if self.selected == Some(index) {
                window.rounded(left + 6, y, list as u32 - 12, ROW as u32 - 2, 6, look.selected);
            }
            let node = self.fs.get(path);
            let is_dir = node.is_some_and(|node| node.is_dir());
            if is_dir {
                folder(window, left + 14, y + 5, 16);
            } else {
                document(window, left + 16, y + 4, &look);
            }
            let size_text = match node {
                Some(node) if !is_dir => size(node.data.len()),
                _ => String::new(),
            };
            let size_width = width_of(&size_text, LABEL) as i32;
            let room = (list - 48 - size_width - 16).max(16) as u32;
            let label = fit(name_of(path), LABEL, room);
            let text_y = y + (ROW - line_height(LABEL) as i32) / 2 - 1;
            window.text(left + 40, text_y, &label, LABEL, look.text);
            window.text(left + list - size_width - 12, text_y, &size_text, LABEL, look.secondary);
        }
        scroller(window, left + list - 5, TOOLBAR + 4, height as i32 - TOOLBAR - 8, top, rows, self.entries.len(), &look);
        if self.entries.is_empty() {
            window.text(left + 40, TOOLBAR + 12, "Empty folder", LABEL, look.secondary);
        }

        // The preview.
        let preview = left + list;
        window.fill(preview, TOOLBAR, 1, height - TOOLBAR as u32, look.separator);
        let chosen_path = self.selected.and_then(|index| self.entries.get(index));
        let chosen = chosen_path.and_then(|path| self.fs.get(path));
        let ran = self.ran.as_ref().filter(|ran| chosen_path == Some(&ran.path));
        let room = (width as i32 - preview - 32).max(16) as u32;
        match chosen {
            Some(node) if !node.is_dir() => {
                let x = preview + 16;
                window.text(x, TOOLBAR + 12, &fit(node.name(), TITLE, room), TITLE, look.text);
                let mut y = TOOLBAR + 14 + line_height(TITLE) as i32;
                let (detail, body) = match ran {
                    Some(ran) => (ran.note.clone(), Some(ran.output.as_str())),
                    None => (size(node.data.len()), core::str::from_utf8(node.data).ok()),
                };
                window.text(x, y, &fit(&detail, LABEL, room), LABEL, look.secondary);
                y += line_height(LABEL) as i32 + 10;
                match body {
                    Some(text) => {
                        // The text on a sheet of paper.
                        let sheet = height as i32 - y - 12;
                        window.rounded(x - 4, y, room + 8, sheet.max(0) as u32, 6, look.paper);
                        let mut line_y = y + 8;
                        let step = line_height(LABEL) as i32 + 2;
                        let fits = ((sheet - 14) / step).max(1) as usize;
                        let total = text.lines().count();
                        self.preview_top = self.preview_top.min(total.saturating_sub(fits));
                        for line in text.lines().skip(self.preview_top) {
                            if line_y + line_height(LABEL) as i32 > y + sheet - 6 {
                                break;
                            }
                            let shown = fit(line, LABEL, room - 8);
                            window.text(x + 4, line_y, &shown, LABEL, 0x2a2a_2e);
                            line_y += step;
                        }
                        scroller(window, x + room as i32 - 1, y + 4, sheet - 8, self.preview_top, fits, total, &look);
                    }
                    None if is_program(node.name()) => {
                        window.text(x, y, "A Windows program", LABEL, look.secondary);
                        let y = y + line_height(LABEL) as i32 + 4;
                        window.text(x, y, "Return runs it", LABEL, look.secondary);
                    }
                    None => {
                        window.text(x, y, "Not text: no preview", LABEL, look.secondary);
                    }
                }
            }
            _ => {
                let note = "Select a file to preview it";
                let note_width = width_of(note, LABEL) as i32;
                let x = preview + ((width as i32 - preview) - note_width) / 2;
                window.text(x, height as i32 / 2, note, LABEL, look.secondary);
            }
        }
        window.commit_all();
    }

    /// A click at `x`, `y`: a place, back, or an entry.
    fn click(&mut self, x: i32, y: i32) {
        if x < SIDEBAR {
            let index = (y - 36).div_euclid(ROW);
            if let Some(place) = usize::try_from(index).ok().and_then(|i| self.places.get(i)) {
                let place = place.clone();
                self.open(&place);
            }
            return;
        }
        if y < TOOLBAR {
            if x < SIDEBAR + 40 && self.cwd != "/" {
                let up = parent_of(&self.cwd);
                self.open(&up);
            }
            return;
        }
        if x < SIDEBAR + self.list_width() {
            let index = (y - TOOLBAR - 4).div_euclid(ROW);
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let index = index + self.top;
            if index < self.entries.len() {
                // A second click on a selected folder opens it, as a
                // double click would.
                let again = self.selected == Some(index);
                self.select(index);
                if again || self.is_dir(&self.entries[index]) {
                    self.activate();
                }
            }
        }
    }

    /// Show the file at `path`: open its folder and select it.
    fn reveal(&mut self, path: &str) {
        if self.fs.get(path).is_none() {
            println!("no {path} to show");
            return;
        }
        let parent = parent_of(path);
        self.open(&parent);
        if let Some(index) = self.entries.iter().position(|entry| entry == path) {
            self.select(index);
            println!("showing {path}");
        }
    }

    /// How many entries the list has room for.
    fn rows(&self) -> usize {
        ((self.window.height as i32 - TOOLBAR - 8) / ROW).max(1) as usize
    }

    /// Select an entry, scrolling the list to show it.
    fn select(&mut self, index: usize) {
        if self.selected != Some(index) {
            self.preview_top = 0;
        }
        self.selected = Some(index);
        let rows = self.rows();
        if index < self.top {
            self.top = index;
        } else if index >= self.top + rows {
            self.top = index + 1 - rows;
        }
    }

    /// The wheel turned with the pointer at `x`: the list or the preview
    /// under it scrolls, three lines a notch, as macOS's does.
    fn scroll(&mut self, notches: i32, x: i32) {
        if x < SIDEBAR {
            return;
        }
        let by = |at: usize| (at as i64 + notches as i64 * 3).max(0) as usize;
        if x < SIDEBAR + self.list_width() {
            let most = self.entries.len().saturating_sub(self.rows());
            self.top = by(self.top).min(most);
        } else {
            // Clamped to the text when it is next drawn.
            self.preview_top = by(self.preview_top);
        }
    }

    fn key(&mut self, key: u8) {
        let count = self.entries.len();
        match key {
            KEY_DOWN if count > 0 => {
                self.select(self.selected.map_or(0, |index| (index + 1).min(count - 1)))
            }
            KEY_UP if count > 0 => self.select(self.selected.map_or(0, |index| index.saturating_sub(1))),
            KEY_ENTER => self.activate(),
            KEY_BACKSPACE if self.cwd != "/" => {
                let up = parent_of(&self.cwd);
                self.open(&up);
            }
            _ => {}
        }
    }
}

/// macOS's overlay scroller: a thin pill along `x`, from `y` for `length`
/// pixels, showing which `shown` of `total` rows from `top` are in view.
/// Nothing when everything fits.
#[allow(clippy::too_many_arguments)]
fn scroller(window: &mut Window, x: i32, y: i32, length: i32, top: usize, shown: usize, total: usize, look: &Look) {
    if total <= shown || length <= 0 {
        return;
    }
    let thumb = (length as i64 * shown as i64 / total as i64).max(20) as i32;
    let travel = length - thumb;
    let most = (total - shown) as i64;
    let at = y + (travel as i64 * top.min(total - shown) as i64 / most) as i32;
    window.rounded(x, at, 4, thumb as u32, 2, 0x8000_0000 | look.secondary);
}

/// A folder icon, `size` wide, with its top left at `x`, `y`.
fn folder(window: &mut Window, x: i32, y: i32, size: u32) {
    let tab = size * 2 / 5;
    window.rounded(x, y, tab, size / 4, 2, FOLDER);
    window.rounded(x, y + 2, size, size * 3 / 4, 3, FOLDER);
    window.rounded(x, y + 4, size, size * 3 / 4 - 2, 3, 0x5a_c8fa);
}

/// A sheet of paper with lines on it.
fn document(window: &mut Window, x: i32, y: i32, look: &Look) {
    window.rounded(x, y, 13, 17, 2, look.paper);
    for line in 0..3 {
        window.fill(x + 3, y + 5 + line * 3, 7, 1, 0xb0b0_b8);
    }
}

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
    Fs::from_tar(bytes).unwrap_or_else(|_| Fs::empty())
}

fn main() {
    corrode_rt::log::set(app::LOG);
    if corrode_rt::heap::init(app::POOL, app::HEAP, HEAP_PAGES).is_err() {
        println!("no memory for a heap");
        corrode_rt::exit();
    }
    let window = match Window::open_titled(
        WIDTH,
        HEIGHT,
        flags::TRANSLUCENT | flags::RESIZABLE,
        "Files",
    ) {
        Ok(window) => window,
        Err(error) => {
            println!("no window: {:?}", error);
            corrode_rt::exit();
        }
    };
    let fs = files();
    let mut places = alloc::vec![String::from("/")];
    places.extend(fs.children("/").filter(|node| node.is_dir()).map(|node| node.path.clone()));
    let mut browser = Browser {
        window,
        fs,
        theme: Theme::default(),
        cwd: String::new(),
        entries: Vec::new(),
        selected: None,
        top: 0,
        preview_top: 0,
        places,
        ran: None,
    };
    browser.open("/");
    browser.draw();

    loop {
        match browser.window.wait(u64::MAX) {
            Event::Theme(theme) => browser.theme = theme,
            Event::Resize { .. } => {
                if let Some(index) = browser.selected {
                    browser.select(index);
                }
            }
            Event::Scroll { notches, x, .. } => browser.scroll(notches, x),
            Event::Reveal(path) => {
                let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
                if let Ok(path) = core::str::from_utf8(&path[..end]) {
                    browser.reveal(path);
                }
            }
            Event::Button { x, y, buttons } if buttons & 1 != 0 => browser.click(x, y),
            Event::Key {
                key, pressed: true, ..
            } => browser.key(key),
            Event::Close => {
                println!("closed");
                corrode_rt::exit();
            }
            _ => continue,
        }
        browser.draw();
    }
}
