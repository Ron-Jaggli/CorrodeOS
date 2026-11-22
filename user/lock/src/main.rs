//! The lock screen: the whole screen under glass, with the time large in
//! the middle, as macOS's and Plasma's are.
//!
//! It opens a cover window, which the desktop puts above everything and
//! gives every key and click, so nothing behind it can be used. Without a
//! passcode, Return or a click on the unlock button takes it away. With
//! one (`passcode =` in /etc/desktop.conf, or the vault's), what is typed --
//! 4 to 32 printable characters -- is put to the desktop, which alone knows
//! the passcode, and only a yes unlocks; after three wrong tries the
//! desktop takes no more for half a minute.

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
/// Return and Backspace, by scancode.
const KEY_ENTER: u8 = 0x1c;
const KEY_BACKSPACE: u8 = 0x0e;
/// The most characters a passcode has (`corrode_theme::passcode::MOST`).
const MOST: usize = 32;
/// The most dots shown: as many as fit the button. More characters show
/// no more dots, which also keeps a long passcode's length to itself.
const MOST_DOTS: usize = 10;
const DOT: u32 = 10;

struct Clock {
    anchor: Option<(i64, u64)>,
    theme: Theme,
    /// Whether a passcode is asked for, what has been typed of it, and what
    /// the last try said.
    passcode: bool,
    typed: alloc::string::String,
    said: Option<alloc::string::String>,
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
    if !clock.passcode {
        centre(window, hy, "Press Return to unlock", HINT, text);
    } else if clock.typed.is_empty() {
        centre(window, hy, "Enter passcode", HINT, text);
    } else {
        // A dot for each character, never the character.
        let count = clock.typed.len().min(MOST_DOTS) as u32;
        let span = count * DOT + (count - 1) * DOT;
        let mut x = bx + (BUTTON.0 - span) as i32 / 2;
        let y = by + (BUTTON.1 - DOT) as i32 / 2;
        for _ in 0..count {
            window.rounded(x, y, DOT, DOT, DOT / 2, text);
            x += 2 * DOT as i32;
        }
    }
    if let Some(said) = &clock.said {
        centre(window, by + BUTTON.1 as i32 + 14, said, HINT, text);
    }
    window.commit_all();
    next
}

/// Return, or the button: unlock if nothing is asked, or put what was typed
/// to the desktop.
fn submit(clock: &mut Clock) {
    if !clock.passcode {
        unlock();
    }
    if clock.typed.is_empty() {
        return;
    }
    let answer = corrode_ui::try_passcode(&clock.typed);
    clock.typed.clear();
    clock.said = Some(match answer {
        Ok(true) => unlock(),
        Ok(false) => {
            println!("wrong passcode");
            alloc::string::String::from("Wrong passcode")
        }
        Err(seconds) => {
            println!("too many tries: {seconds} s to wait");
            format!("Too many tries - wait {seconds} s")
        }
    });
}

fn unlock() -> ! {
    println!("unlocked");
    corrode_rt::exit();
}

/// Where the page for [`probe_sealing`] goes.
const SEAL_PAGE: usize = 9;

/// Seal a short message with this program's sealer and open it again, to
/// show the path from ring 3 to the secure core works. Nothing is kept: the
/// key is new every boot, so a sealed message is good for this boot only.
fn probe_sealing() {
    const PROBE: &[u8] = b"held by the lock screen";
    const AT: usize = corrode_rt::abi::seal::OVERHEAD;
    let result = (|| -> corrode_rt::Result<(usize, bool)> {
        corrode_rt::alloc(app::POOL, 1, SEAL_PAGE)?;
        let page = corrode_rt::map(SEAL_PAGE)?;
        // SAFETY: the kernel mapped this page writable for this task alone.
        let bytes = unsafe { core::slice::from_raw_parts_mut(page.address, page.len) };
        bytes[AT..AT + PROBE.len()].copy_from_slice(PROBE);
        let sealed = corrode_rt::seal(app::SEAL, SEAL_PAGE, PROBE.len())?;
        let hidden = !bytes[..sealed].windows(PROBE.len()).any(|w| w == PROBE);
        let opened = corrode_rt::unseal(app::SEAL, SEAL_PAGE, sealed)?;
        let back = opened == PROBE.len() && &bytes[AT..AT + opened] == PROBE;
        bytes.fill(0);
        Ok((sealed, hidden && back))
    })();
    let _ = corrode_rt::delete(SEAL_PAGE);
    match result {
        Ok((sealed, true)) => println!("sealed and opened {} bytes ({} sealed)", PROBE.len(), sealed),
        Ok((_, false)) => println!("sealing: the message did not come back as it went in"),
        Err(error) => println!("sealing: {:?}", error),
    }
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
        passcode: corrode_ui::passcode_needed(),
        typed: alloc::string::String::new(),
        said: None,
    };
    let mut next = draw(&mut window, &clock);
    println!("locked, {}x{}", window.width, window.height);
    probe_sealing();

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
            } => {
                submit(&mut clock);
                next = draw(&mut window, &clock);
            }
            Event::Key {
                key: KEY_BACKSPACE,
                pressed: true,
                ..
            } if clock.passcode => {
                clock.typed.pop();
                next = draw(&mut window, &clock);
            }
            // Printable, no space and no `#`: what a passcode can hold
            // (`corrode_theme::passcode::allowed`).
            Event::Key {
                text: Some(character),
                pressed: true,
                ..
            } if clock.passcode
                && clock.typed.len() < MOST
                && character.is_ascii_graphic()
                && character != '#' =>
            {
                clock.typed.push(character);
                clock.said = None;
                next = draw(&mut window, &clock);
            }
            Event::Button { x, y, buttons } if buttons & 1 != 0 => {
                let (bx, by) = button(&window);
                if x >= bx && x < bx + BUTTON.0 as i32 && y >= by && y < by + BUTTON.1 as i32 {
                    submit(&mut clock);
                    next = draw(&mut window, &clock);
                }
            }
            _ => {}
        }
    }
}
