//! Files: the root filesystem, as Finder shows a disk.
//!
//! A sidebar of places -- the root and each directory in it -- a list of
//! what is in the current directory, and a preview of the file selected in
//! it: its text, if it is text. Folders open with a click, or with Return;
//! the arrows move the selection and Backspace goes up a level.
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
    /// The sidebar's places: the root, then each directory in it.
    places: Vec<String>,
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
        println!("{} ({} items)", self.cwd, self.entries.len());
    }

    fn is_dir(&self, path: &str) -> bool {
        self.fs.get(path).is_some_and(|node| node.is_dir())
    }

    /// Open the selected entry: into a folder, or nothing for a file, whose
    /// preview is already showing.
    fn activate(&mut self) {
        let Some(path) = self.selected.and_then(|index| self.entries.get(index)).cloned() else {
            return;
        };
        if self.is_dir(&path) {
            self.open(&path);
        }
    }

    fn list_width(&self) -> i32 {
        (self.window.width as i32 - SIDEBAR) * 55 / 100
    }

    fn draw(&mut self) {
        let look = Look::of(&self.theme);
        let (width, height) = (self.window.width, self.window.height);
        let list = self.list_width();
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
        let rows = ((height as i32 - TOOLBAR - 8) / ROW).max(0) as usize;
        for (index, path) in self.entries.iter().take(rows).enumerate() {
            let y = TOOLBAR + 4 + index as i32 * ROW;
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
        if self.entries.is_empty() {
            window.text(left + 40, TOOLBAR + 12, "Empty folder", LABEL, look.secondary);
        }

        // The preview.
        let preview = left + list;
        window.fill(preview, TOOLBAR, 1, height - TOOLBAR as u32, look.separator);
        let chosen = self
            .selected
            .and_then(|index| self.entries.get(index))
            .and_then(|path| self.fs.get(path));
        let room = (width as i32 - preview - 32).max(16) as u32;
        match chosen {
            Some(node) if !node.is_dir() => {
                let x = preview + 16;
                window.text(x, TOOLBAR + 12, &fit(node.name(), TITLE, room), TITLE, look.text);
                let mut y = TOOLBAR + 14 + line_height(TITLE) as i32;
                window.text(x, y, &size(node.data.len()), LABEL, look.secondary);
                y += line_height(LABEL) as i32 + 10;
                match core::str::from_utf8(node.data) {
                    Ok(text) => {
                        // The text on a sheet of paper.
                        let sheet = height as i32 - y - 12;
                        window.rounded(x - 4, y, room + 8, sheet.max(0) as u32, 6, look.paper);
                        let mut line_y = y + 8;
                        for line in text.lines() {
                            if line_y + line_height(LABEL) as i32 > y + sheet - 6 {
                                break;
                            }
                            let shown = fit(line, LABEL, room - 8);
                            window.text(x + 4, line_y, &shown, LABEL, 0x2a2a_2e);
                            line_y += line_height(LABEL) as i32 + 2;
                        }
                    }
                    Err(_) => {
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
            if index < self.entries.len() {
                // A second click on a selected folder opens it, as a
                // double click would.
                let again = self.selected == Some(index);
                self.selected = Some(index);
                if again || self.is_dir(&self.entries[index]) {
                    self.activate();
                }
            }
        }
    }

    fn key(&mut self, key: u8) {
        let count = self.entries.len();
        match key {
            KEY_DOWN if count > 0 => {
                self.selected = Some(self.selected.map_or(0, |index| (index + 1).min(count - 1)))
            }
            KEY_UP if count > 0 => {
                self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)))
            }
            KEY_ENTER => self.activate(),
            KEY_BACKSPACE if self.cwd != "/" => {
                let up = parent_of(&self.cwd);
                self.open(&up);
            }
            _ => {}
        }
    }
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
        places,
    };
    browser.open("/");
    browser.draw();

    loop {
        match browser.window.wait(u64::MAX) {
            Event::Theme(theme) => browser.theme = theme,
            Event::Resize { .. } => {}
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
