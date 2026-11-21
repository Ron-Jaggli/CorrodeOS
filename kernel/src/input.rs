//! Keyboard and mouse, from the PS/2 driver in ring 3 to the screen.
//!
//! The driver sends one message per key or mouse movement to an endpoint. A
//! kernel task holds the receive half and puts what arrives in a queue; the
//! painter empties the queue when it wakes and changes the scene to match.
//! The task exists because a send is a rendezvous and the painter does not
//! block: it sleeps until the next tick, then paints whatever has happened.
//!
//! The painter's half lives here too, as [`Desk`]: a pointer that follows the
//! mouse and stays on top, and the window manager. A program's window gets a
//! frame of glass with a title bar, macOS's three buttons and a shadow;
//! clicking a window gives it the keyboard and brings it to the front, and
//! dragging its bar moves it. Keys go to the window with the
//! keyboard, or, when none has it, to a line of typed text on the boot
//! screen.

use alloc::string::String;
use alloc::vec::Vec;

use corrode_abi::{launcher, ps2, window};
use corrode_render::anim::{Animation, Ease, Fx, Spring};
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_text::{Coverage, Style};
use corrode_theme::{Buttons, Theme};

use crate::arch::x86_64::without_interrupts;
use crate::cap::CapPtr;
use crate::ipc;
use crate::println;
use crate::sync::SpinLock;

/// Something the keyboard or the mouse did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The driver has set the controller up and says which devices answered.
    Ready { keyboard: bool, mouse: bool },
    /// A key went down or came up, and what it types.
    Key {
        key: u8,
        pressed: bool,
        text: Option<char>,
        modifiers: u8,
    },
    /// The mouse moved, or its buttons changed.
    Pointer {
        dx: i32,
        dy: i32,
        buttons: u8,
        wheel: i32,
    },
}

/// Events waiting for the painter.
const QUEUE: usize = 128;

struct Queue {
    events: [Option<Event>; QUEUE],
    head: usize,
    len: usize,
    /// Events that arrived to a full queue and were dropped.
    dropped: u64,
}

impl Queue {
    fn push(&mut self, event: Event) {
        // Movements with the same buttons held add up: the painter wants to
        // know where the mouse is, not every step it took to get there. A
        // queue full of a fast mouse would otherwise drop key presses.
        if let (
            Event::Pointer {
                dx,
                dy,
                buttons,
                wheel,
            },
            Some(Some(Event::Pointer {
                dx: last_x,
                dy: last_y,
                buttons: last_buttons,
                wheel: last_wheel,
            })),
        ) = (event, self.last_mut().map(|last| *last))
        {
            if buttons == last_buttons {
                if let Some(last) = self.last_mut() {
                    *last = Some(Event::Pointer {
                        dx: last_x.saturating_add(dx),
                        dy: last_y.saturating_add(dy),
                        buttons,
                        wheel: last_wheel.saturating_add(wheel),
                    });
                }
                return;
            }
        }
        if self.len == QUEUE {
            self.dropped += 1;
            return;
        }
        self.events[(self.head + self.len) % QUEUE] = Some(event);
        self.len += 1;
    }

    fn last_mut(&mut self) -> Option<&mut Option<Event>> {
        if self.len == 0 {
            return None;
        }
        Some(&mut self.events[(self.head + self.len - 1) % QUEUE])
    }

    fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.events[self.head].take();
        self.head = (self.head + 1) % QUEUE;
        self.len -= 1;
        event
    }
}

/// Shared between the receiving task and the painter. Locked with interrupts
/// off, like everything the timer can preempt.
static QUEUE_STATE: SpinLock<Queue> = SpinLock::new(Queue {
    events: [None; QUEUE],
    head: 0,
    len: 0,
    dropped: 0,
});

/// The receiving task's one capability: the receive half of the endpoint
/// the driver sends to.
pub const TASK_ENDPOINT: CapPtr = 0;

/// Turn a message from the driver into an event. Anything else is ignored:
/// the endpoint's send half was given to the driver alone, but a message is
/// still numbers from ring 3.
fn decode(message: &ipc::Message) -> Option<Event> {
    let words = message.words;
    match message.label {
        ps2::LABEL_READY => Some(Event::Ready {
            keyboard: words[0] != 0,
            mouse: words[1] != 0,
        }),
        ps2::LABEL_KEY => Some(Event::Key {
            key: words[0] as u8,
            modifiers: words[3] as u8,
            pressed: words[1] != 0,
            text: u32::try_from(words[2])
                .ok()
                .filter(|code| *code != 0)
                .and_then(char::from_u32),
        }),
        // Clamped to what a mouse can report in one packet, so a driver
        // that lies about how far it went cannot overflow anything.
        ps2::LABEL_POINTER => Some(Event::Pointer {
            dx: (words[0] as i64).clamp(-256, 255) as i32,
            dy: (words[1] as i64).clamp(-256, 255) as i32,
            buttons: (words[2] & 0x07) as u8,
            wheel: (words[3] as i64).clamp(-8, 7) as i32,
        }),
        _ => None,
    }
}

/// Receive from the driver, for ever.
pub extern "C" fn receiver() {
    loop {
        let Ok(received) = ipc::recv(TASK_ENDPOINT, None, None) else {
            return;
        };
        if let Some(event) = decode(&received.message) {
            without_interrupts(|| QUEUE_STATE.lock().push(event));
        }
    }
}

/// Take everything waiting, oldest first.
fn drain() -> Vec<Event> {
    without_interrupts(|| {
        let mut queue = QUEUE_STATE.lock();
        let mut out = Vec::with_capacity(queue.len);
        while let Some(event) = queue.pop() {
            out.push(event);
        }
        out
    })
}

/// Where the pointer and the typed line keep their stencils in the mask
/// atlas: the lower half, below anything the boot screen puts there.
pub const ATLAS_ROW: u32 = 256;
/// The pointer's box, with room around the arrow for its outline.
const POINTER_SIZE: (u32, u32) = (16, 22);
/// The typed line's block: the atlas's whole width, enough rows for the
/// style below.
const LINE_ROW: u32 = ATLAS_ROW + 32;
const LINE_STYLE: Style = Style::px(22);
/// How much may be typed before more is refused.
const LINE_MAX: usize = 60;
const LINE_COLOR: Color = Color::rgb(0xf2, 0xee, 0xe8);

/// The arrow, as how much of each pixel it covers out of 255, with its tip
/// one pixel in from the top left so the outline has room.
///
/// A triangle -- tip, straight down, and out to the lower right -- sampled
/// four by four per pixel. The outline is the same triangle with every edge
/// moved out by `grow` pixels.
fn arrow(grow: i32) -> Vec<u8> {
    let (width, height) = POINTER_SIZE;
    let mut coverage = alloc::vec![0u8; (width * height) as usize];
    // Corners in sixteenths of a pixel, so every sample is a whole number,
    // and each edge's length in the same units, for moving it outwards.
    let corners = [(16, 16), (16, 16 + 16 * 16), (16 + 16 * 11, 16 + 16 * 11)];
    let lengths = [256, 193, 249];
    let edge = |from: (i32, i32), to: (i32, i32), x: i32, y: i32| {
        (to.0 - from.0) * (y - from.1) - (to.1 - from.1) * (x - from.0)
    };
    let inside = |x: i32, y: i32| {
        (0..3).all(|i| {
            let (from, to) = (corners[i], corners[(i + 1) % 3]);
            // The corners go round anticlockwise on a screen whose y runs
            // down, so inside is where every edge function is negative.
            -edge(from, to, x, y) >= -grow * 16 * lengths[i]
        })
    };
    for py in 0..height as i32 {
        for px in 0..width as i32 {
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    if inside(px * 16 + sx * 4 + 2, py * 16 + sy * 4 + 2) {
                        hits += 1;
                    }
                }
            }
            coverage[(py as u32 * width + px as u32) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// What the pointer looks like: the arrow, or a double-headed arrow over an
/// edge that would resize the window, pointing the ways it would go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cursor {
    Arrow,
    /// Left and right.
    Across,
    /// Up and down.
    UpDown,
    /// Top left and bottom right.
    Falling,
    /// Bottom left and top right.
    Rising,
}

/// A resize cursor's box, with its hot spot in the middle.
const RESIZE_SIZE: u32 = 21;
/// Where the resize cursors keep their stencils: along the pointer's row,
/// after the window buttons' -- an outline and a fill each.
const RESIZE_X: i32 = 96;

impl Cursor {
    const RESIZING: [Cursor; 4] = [Cursor::Across, Cursor::UpDown, Cursor::Falling, Cursor::Rising];

    /// The stencils, the outline's then the fill's, and the hot spot.
    fn stencils(self) -> ([Rect; 2], (i32, i32)) {
        let (width, height) = POINTER_SIZE;
        let Some(slot) = Self::RESIZING.iter().position(|cursor| *cursor == self) else {
            let row = ATLAS_ROW as i32;
            let outline = Rect::new(0, row, width, height);
            let fill = Rect::new(width as i32, row, width, height);
            // The tip is a pixel in from the stencil's corner.
            return ([outline, fill], (1, 1));
        };
        let size = RESIZE_SIZE;
        let x = RESIZE_X + (slot as i32 * 2) * size as i32;
        let half = size as i32 / 2;
        (
            [
                Rect::new(x, ATLAS_ROW as i32, size, size),
                Rect::new(x + size as i32, ATLAS_ROW as i32, size, size),
            ],
            (half, half),
        )
    }

    /// Which edges a press would pick up: left, top, right, bottom.
    fn of(edges: (bool, bool, bool, bool)) -> Cursor {
        match edges {
            (true, true, ..) | (.., true, true) => Cursor::Falling,
            (_, true, true, _) | (true, _, _, true) => Cursor::Rising,
            (true, ..) | (_, _, true, _) => Cursor::Across,
            (_, true, ..) | (.., true) => Cursor::UpDown,
            _ => Cursor::Arrow,
        }
    }
}

/// A double-headed arrow across a `RESIZE_SIZE` box, along `cursor`'s way,
/// as coverage out of 255; the outline is it grown by `grow` pixels.
fn resize_arrow(cursor: Cursor, grow: i32) -> Vec<u8> {
    let size = RESIZE_SIZE as i32;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In sixteenths of a pixel from the middle. Diagonals are turned by
    // 45 degrees with 181/256, near enough to one over root two.
    let half = size * 8;
    let along = |x: i32, y: i32| -> (i32, i32) {
        match cursor {
            Cursor::Across => (x, y),
            Cursor::UpDown => (y, x),
            Cursor::Falling => ((x + y) * 181 / 256, (y - x) * 181 / 256),
            _ => ((x - y) * 181 / 256, (x + y) * 181 / 256),
        }
    };
    let grow = grow * 16;
    // Tips 9 pixels out; heads 5 long and 5 wide either side; a shaft 1
    // wide either side between them.
    let (tip, head, wing, shaft) = (9 * 16, 5 * 16, 5 * 16, 16);
    let inside = |x: i32, y: i32| {
        let (u, v) = along(x - half, y - half);
        let (u, v) = (u.abs(), v.abs());
        let in_shaft = u <= tip - head + grow && v <= shaft + grow;
        // The head narrows from `wing` at its base to nothing at the tip.
        let in_head = u <= tip + grow
            && u >= tip - head - grow
            && v * head <= (tip - u) * wing + grow * (head + wing);
        in_shaft || in_head
    };
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    hits += inside(px * 16 + sx * 4 + 2, py * 16 + sy * 4 + 2) as u32;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// How long a closed window takes to fade out.
const CLOSING_MS: u32 = 180;

/// Ask the launcher for `program`.
fn launch(program: u64) {
    let answer = crate::ipc::call(
        crate::LAUNCHER_ENDPOINT,
        crate::ipc::Message {
            label: launcher::LAUNCH,
            words: [program, 0, 0, 0],
        },
        None,
        None,
    );
    if answer.is_err() {
        println!("desktop    : no launcher to start program {}", program);
    }
}

/// Where a window dragged to an edge of the screen goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Snap {
    Left,
    Right,
    Fill,
}

/// A window gone from the desk but still fading off the screen.
struct Fading {
    layers: Vec<LayerId>,
    surface: usize,
    slot: Option<usize>,
    until: u64,
    on_top: bool,
}

/// Where a popover's top edge hangs: just under the menu bar.
const POPOVER_TOP: i32 = 28 + 6;
/// How large, in percent, a window starts as it zooms in to open.
const ZOOM_FROM: i32 = 90;

/// Height of a window's title bar.
const BAR: u32 = 28;

/// The window buttons: a dot each, in the order a title bar shows them.
const LIGHT: u32 = 12;
/// Centre to centre.
const LIGHT_STEP: i32 = 20;
/// From the bar's nearer end to the nearest dot's edge.
const LIGHT_INSET: i32 = 14;

/// What each button does, and its colour, macOS's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Close,
    Minimize,
    Zoom,
}

impl Button {
    /// Along the bar from the left: macOS's order on the left, Plasma's on
    /// the right.
    const fn order(buttons: Buttons) -> [Button; 3] {
        match buttons {
            Buttons::Left => [Button::Close, Button::Minimize, Button::Zoom],
            Buttons::Right => [Button::Minimize, Button::Zoom, Button::Close],
        }
    }

    const fn color(self) -> Color {
        match self {
            Button::Close => Color::rgb(0xff, 0x5f, 0x57),
            Button::Minimize => Color::rgb(0xfe, 0xbc, 0x2e),
            Button::Zoom => Color::rgb(0x28, 0xc8, 0x40),
        }
    }
}

/// Where each button's stencil is in the atlas: to the right of the
/// pointer's two, the dot and then the glyph drawn over it on hover.
const DOT_SRC: Rect = Rect::new(2 * POINTER_SIZE.0 as i32, ATLAS_ROW as i32, LIGHT, LIGHT);
const GLYPH_SRC: [Rect; 3] = [
    Rect::new(2 * POINTER_SIZE.0 as i32 + LIGHT as i32, ATLAS_ROW as i32, LIGHT, LIGHT),
    Rect::new(2 * POINTER_SIZE.0 as i32 + 2 * LIGHT as i32, ATLAS_ROW as i32, LIGHT, LIGHT),
    Rect::new(2 * POINTER_SIZE.0 as i32 + 3 * LIGHT as i32, ATLAS_ROW as i32, LIGHT, LIGHT),
];
const GLYPH_COLOR: Color = Color::rgb(0x4d, 0x1f, 0x10);

/// Title text: slots in the left half of the atlas below the typed line,
/// two across. The dock keeps its icons in the right half.
const TITLE_ROW: u32 = ATLAS_ROW + 64;
const TITLE_SLOT: (u32, u32) = (256, 22);

/// How soon a second click must follow the first to count as a double one.
const DOUBLE_CLICK_NS: u64 = 400_000_000;
/// Slots between the typed line and the dock's magnified icons, and then
/// more under those, at the foot of the atlas: enough for a title on every
/// window the renderer has surfaces for, with some to spare for menus.
const TITLE_SLOTS_ABOVE: usize = 2 * ((512 - TITLE_ROW) / TITLE_SLOT.1) as usize;
const TITLE_ROW_BELOW: u32 = 768;
const TITLE_SLOTS: usize =
    TITLE_SLOTS_ABOVE + 2 * ((crate::ATLAS_HEIGHT - TITLE_ROW_BELOW) / TITLE_SLOT.1) as usize;
const _: () = assert!(TITLE_SLOTS >= corrode_render::software::SURFACES && TITLE_SLOTS <= 64);
const TITLE_STYLE: Style = Style::px(14).weight(corrode_text::Weight::Bold);

/// How far a window's shadow spreads, and how far down it falls. The
/// shadow is cut out where its shape is, so a drop would leave an unshaded
/// strip under the window; it stays put until shadows can be cut by the
/// window rather than by themselves.
const SHADOW_BLUR: u8 = 26;
const SHADOW_DROP: i32 = 0;


/// One of the window buttons' stencils, as coverage out of 255: a disc,
/// or the glyph drawn over it -- a cross, a bar, a plus -- each sampled
/// four by four per pixel.
fn stencil(kind: Option<Button>) -> Vec<u8> {
    let size = LIGHT as i32;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In eighths of a pixel, from the centre.
    let half = size * 4;
    let inside = |x: i32, y: i32| -> bool {
        let (dx, dy) = (x - half, y - half);
        match kind {
            None => dx * dx + dy * dy <= (half - 2) * (half - 2),
            Some(Button::Close) => {
                dx.abs() <= 22 && dy.abs() <= 22 && ((dx - dy).abs() <= 7 || (dx + dy).abs() <= 7)
            }
            Some(Button::Minimize) => dx.abs() <= 24 && dy.abs() <= 5,
            Some(Button::Zoom) => {
                (dx.abs() <= 24 && dy.abs() <= 5) || (dy.abs() <= 24 && dx.abs() <= 5)
            }
        }
    };
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    hits += inside(px * 8 + sx * 2 + 1, py * 8 + sy * 2 + 1) as i32;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// The Alt key: what Cmd is on macOS, and what Plasma's shortcuts use.
const ALT: u8 = 1 << 2;
const SHIFT: u8 = 1 << 0;
/// Scancodes the switcher listens for.
const KEY_TAB: u8 = 0x0f;
const KEY_ESCAPE: u8 = 0x01;
const KEY_LEFT_ALT: u8 = 0x38;
const KEY_RIGHT_ALT: u8 = 0x80 | 0x38;
/// And search.
const KEY_SPACE: u8 = 0x39;
const KEY_ENTER: u8 = 0x1c;
const KEY_BACKSPACE: u8 = 0x0e;
const KEY_UP: u8 = 0x80 | 0x48;
const KEY_DOWN: u8 = 0x80 | 0x50;

/// What the desktop does with Alt and a key, whatever window is in front.
/// The desktop's Alt shortcuts. Alt+Tab is the switcher's, not here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shortcut {
    /// Alt+Q, Alt+W, Alt+F4: ask the window in front to close.
    Close,
    /// Alt+M.
    Minimize,
    /// Alt+Z.
    Zoom,
    /// Alt+comma, as Cmd+comma opens an app's settings on macOS.
    Settings,
    /// Alt+L: lock the screen.
    Lock,
    /// Alt+Left, Alt+Right, Alt+Up: the window in front to a half of the
    /// screen, or all of it, as dragging it to that edge does.
    Snap(Snap),
    /// Alt+Down: a snapped or zoomed window back where it was.
    Unsnap,
    /// Alt+D: every window out of the way, and again to bring them back,
    /// as Plasma's Peek at Desktop and macOS's Show Desktop do.
    ShowDesktop,
}

impl Shortcut {
    /// By scancode, set 1.
    fn of(key: u8) -> Option<Shortcut> {
        Some(match key {
            0x10 | 0x11 | 0x3e => Shortcut::Close,
            0x32 => Shortcut::Minimize,
            0x2c => Shortcut::Zoom,
            0x33 => Shortcut::Settings,
            0x26 => Shortcut::Lock,
            0xcb => Shortcut::Snap(Snap::Left),
            0xcd => Shortcut::Snap(Snap::Right),
            0xc8 => Shortcut::Snap(Snap::Fill),
            0xd0 => Shortcut::Unsnap,
            0x20 => Shortcut::ShowDesktop,
            _ => return None,
        })
    }
}

/// How tall a window's glass frame is. Under an opaque body the glass
/// would be painted over, and frosting is most of a frame's cost, so the
/// frame is only the title bar and one corner's radius more: enough to
/// reach under the body's square top edge, whose own rounding is only at
/// the bottom. A translucent body, or a window without a bar, shows the
/// glass through, so has all of it; and so does any window while it fades
/// (see [`Managed::fade_frame`]).
fn frame_height(body: u32, bar: u32, translucent: bool, radius: u32) -> u32 {
    if translucent || bar == 0 {
        body + bar
    } else {
        bar + radius.min(body)
    }
}

/// A pane of the desktop's material at `rect`: glass, or a solid colour
/// where the theme has glass off.
pub(crate) fn pane(rect: Rect, radius: u16, theme: &Theme) -> Layer {
    match theme.pane(radius) {
        Some(glass) => Layer::glass(rect, glass),
        None => Layer::solid(rect, theme.surface()),
    }
}

/// A window, as the painter keeps it.
struct Managed {
    window: usize,
    surface: usize,
    docked: bool,
    /// Under the menu bar, with no title bar; closes when anything else
    /// is clicked.
    popover: bool,
    /// The whole screen, above everything, with every key and click.
    cover: bool,
    /// Where the window's top left is -- its title bar's, if it has one.
    at: (i32, i32),
    width: u32,
    height: u32,
    /// Under everything else of it.
    shadow: Option<LayerId>,
    /// The glass behind the title bar and the body: the whole window.
    frame: Option<LayerId>,
    /// The buttons, in the order they sit along the bar.
    lights: [Option<(Button, LayerId)>; 3],
    /// The title, its slot in the atlas, and how wide it came out.
    title: Option<(LayerId, usize, u32)>,
    /// What the title says, for the dock to know what is running.
    name: String,
    /// Put away by its yellow button, until its dock icon is clicked.
    minimized: bool,
    /// Where it was, and how big, before its green button zoomed it, to go
    /// back to.
    unzoomed: Option<((i32, i32), (u32, u32))>,
    body: LayerId,
    /// When the fade it is in ends, if it is in one: until then its frame
    /// is the whole window's height.
    fade_until: Option<u64>,
}

/// What part of a window a layer is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Shadow,
    /// The title bar: the frame, or the title on it.
    Bar,
    Light(Button),
    Body,
}

impl Managed {
    /// How tall its frame is now: see [`frame_height`].
    fn frame_rows(&self, radius: u32) -> u32 {
        let whole = self.fade_until.is_some() || crate::windows::translucent(self.window);
        frame_height(self.height, self.bar(), whole, radius)
    }

    /// Its frame the whole window's height until `until`, for a fade. A
    /// frame only a radius deeper than the bar ends under the body, and a
    /// body less than opaque shows that edge through it: a line under the
    /// title bar. The desk trims the frame back once the fade is over.
    fn fade_frame<const N: usize>(&mut self, scene: &mut Scene<N>, until: u64) {
        self.fade_until = Some(until);
        let whole = frame_height(self.height, self.bar(), true, 0) as i32;
        if let Some(layer) = self.frame.and_then(|id| scene.layer_mut(id)) {
            // Already growing to it, as it does while it zooms in: left be.
            if layer.height.target().round() != whole {
                layer.height = Animation::rest(Fx::from_int(whole));
            }
        }
    }

    fn bar(&self) -> u32 {
        if self.docked || self.popover || self.cover {
            0
        } else {
            BAR
        }
    }

    /// Every layer of the window, bottom first.
    fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        self.shadow
            .into_iter()
            .chain(self.frame)
            .chain(self.lights.iter().flatten().map(|(_, id)| *id))
            .chain(self.title.map(|(id, ..)| id))
            .chain(Some(self.body))
    }

    /// Which part of the window `id` is, if it is any.
    fn part(&self, id: LayerId) -> Option<Part> {
        if self.shadow == Some(id) {
            Some(Part::Shadow)
        } else if self.body == id {
            Some(Part::Body)
        } else if self.frame == Some(id) || self.title.is_some_and(|(title, ..)| title == id) {
            Some(Part::Bar)
        } else {
            self.lights
                .iter()
                .flatten()
                .find(|(_, light)| *light == id)
                .map(|(button, _)| Part::Light(*button))
        }
    }

    /// Where the `index`th button along the bar goes, for a window at `at`.
    fn light_at(&self, at: (i32, i32), index: usize, buttons: Buttons) -> (i32, i32) {
        let y = at.1 + (BAR - LIGHT) as i32 / 2;
        let x = match buttons {
            Buttons::Left => at.0 + LIGHT_INSET + index as i32 * LIGHT_STEP,
            Buttons::Right => {
                at.0 + self.width as i32 - LIGHT_INSET - LIGHT as i32 - (2 - index as i32) * LIGHT_STEP
            }
        };
        (x, y)
    }

    /// Every layer and where its top left goes, for a window at `at`.
    fn places(&self, at: (i32, i32), buttons: Buttons) -> Vec<(LayerId, (i32, i32))> {
        let mut places = Vec::new();
        let blur = SHADOW_BLUR as i32;
        if let Some(shadow) = self.shadow {
            places.push((shadow, (at.0 - blur, at.1 - blur + SHADOW_DROP)));
        }
        if let Some(frame) = self.frame {
            places.push((frame, at));
        }
        for (index, light) in self.lights.iter().enumerate() {
            if let Some((_, id)) = light {
                places.push((*id, self.light_at(at, index, buttons)));
            }
        }
        if let Some((title, _, width)) = self.title {
            let height = corrode_text::measure("Ag", TITLE_STYLE).height;
            places.push((
                title,
                (
                    at.0 + (self.width as i32 - width as i32) / 2,
                    at.1 + (BAR as i32 - height as i32) / 2,
                ),
            ));
        }
        places.push((self.body, (at.0, at.1 + self.bar() as i32)));
        places
    }
}

/// The keyboard and mouse, as the scene shows them, and the windows they
/// act on.
pub struct Desk {
    /// Outline below, fill above, moving together.
    pointer: Option<[LayerId; 2]>,
    at: (i32, i32),
    buttons: u8,
    line: Option<LayerId>,
    typed: String,
    line_height: u32,
    /// Whether the first movement has been reported.
    moved: bool,
    /// Whether the buttons' stencils made it into the atlas.
    stencils: bool,
    /// The glyphs shown over the buttons of the window the pointer is over:
    /// one set, moved to wherever it is wanted.
    glyphs: [Option<LayerId>; 3],
    /// Which window's buttons they are over now.
    hovering: Option<usize>,
    /// Title slots in the atlas in use, one bit each.
    titles: u64,
    /// Along the bottom of the screen.
    dock: Option<crate::dock::Dock>,
    /// The Alt+Tab panel, while Alt is held.
    switcher: Option<crate::switcher::Switcher>,
    /// Alt+Space's search field, while it is up.
    spotlight: Option<crate::spotlight::Spotlight>,
    /// A file search asked Files to show, for when its window opens.
    reveal: Option<[u64; 4]>,
    /// The notification on screen, if any.
    banner: Option<crate::banner::Banner>,
    /// Window ids, the one most recently in front first: the switcher's
    /// order.
    recent: Vec<usize>,
    theme: Theme,
    windows: Vec<Managed>,
    /// Which of `windows` has the keyboard.
    focus: Option<usize>,
    /// A window being dragged by its bar, and where on the bar it was held.
    drag: Option<(usize, i32, i32)>,
    /// Where the dragged window was when it was picked up, to go back to
    /// after a snap is undone by zoom.
    drag_from: (i32, i32),
    /// The edge a dragged window would snap to, and the preview showing
    /// where.
    snap: Option<(Snap, LayerId)>,
    /// The window picked up was snapped or zoomed: the first move of the
    /// drag gives it back its size from before.
    unsnap: bool,
    /// Where the desktop was last right-clicked: where its menu opens.
    menu_at: (i32, i32),
    /// The menu goes above `menu_at`, centred on it: a dock icon's menu.
    menu_above: bool,
    /// The last press on a title bar: which window, when, and where, so a
    /// second one soon after on the same spot zooms it.
    bar_click: Option<(usize, u64, (i32, i32))>,
    /// The windows Show Desktop put away, by id, to bring back.
    desktop_shown: Vec<usize>,
    /// Where a popover was last told the pointer is, so it is told once.
    hovered_at: Option<(usize, i32, i32)>,
    /// Windows placed so far, for staggering the next one.
    placed: i32,
    /// The surfaces of windows that have gone, for the painter to retire.
    closed: Vec<usize>,
    /// Windows gone but still fading out: their layers, surface and title
    /// slot, when the fade ends, and whether it stays above everything (a
    /// cover does, so the lock screen does not sink under the dock).
    fading: Vec<Fading>,
    /// What docked windows were last told has the keyboard.
    announced: Option<String>,
    /// Whether the renderer can be shown a surface again at a new size.
    can_resize: bool,
    /// Surfaces resized since the painter last asked, to show again.
    resized: Vec<usize>,
    /// A window being resized by an edge: which, where the pointer and the
    /// size were when it was picked up, and which edges move.
    sizing: Option<Sizing>,
    /// What the pointer looks like now.
    cursor: Cursor,
    /// Whether the resize cursors' stencils are there to show.
    resize_cursors: bool,
    /// The size the pointer has dragged it to, applied once a poll.
    /// With where its right and bottom edges are to stay, when its left
    /// or top edge is being dragged.
    wanted: Option<(usize, (u32, u32), (Option<i32>, Option<i32>))>,
}

/// A resize by the edges, in progress.
#[derive(Clone, Copy)]
struct Sizing {
    index: usize,
    from: (i32, i32),
    size: (u32, u32),
    /// Which edges follow the pointer. A left or top edge moves the window
    /// as well, so that its right or bottom edge stays where it is.
    left: bool,
    top: bool,
    right: bool,
    bottom: bool,
}

/// How far in from a resizable window's edges a press picks the edge up
/// rather than going to the program.
const EDGE: i32 = 8;
/// The top edge is the title bar's top few pixels: the rest of the bar
/// drags the window.
const TOP_EDGE: i32 = 4;
/// The smallest a window is resized to by hand.
const MIN_SIZE: (u32, u32) = (240, 120);

impl Desk {
    /// Put the pointer's stencils in the atlas and its layers in the scene,
    /// hidden until a mouse turns up, and an empty line to type on.
    pub fn new<R: Renderer, const N: usize>(renderer: &mut R, scene: &mut Scene<N>) -> Self {
        let screen = scene.screen();
        let (width, height) = POINTER_SIZE;
        let mut pointer = None;
        let outline = Rect::new(0, ATLAS_ROW as i32, width, height);
        let fill = Rect::new(width as i32, ATLAS_ROW as i32, width, height);
        // The resize cursors are a nicety: without them the arrow stays.
        let resizing = Cursor::RESIZING.iter().all(|cursor| {
            let ([outline, fill], _) = cursor.stencils();
            renderer.upload_mask(outline, &resize_arrow(*cursor, 1)).is_ok()
                && renderer.upload_mask(fill, &resize_arrow(*cursor, 0)).is_ok()
        });
        if renderer.upload_mask(outline, &arrow(1)).is_ok()
            && renderer.upload_mask(fill, &arrow(0)).is_ok()
        {
            let at = (screen.width as i32 / 2, screen.height as i32 / 2);
            let mut layers = [
                Layer::mask(at.0, at.1, outline, Color::BLACK),
                Layer::mask(at.0, at.1, fill, Color::WHITE),
            ];
            for layer in &mut layers {
                layer.opacity = Animation::rest(Fx::ZERO);
            }
            if let (Some(below), Some(above)) = (scene.add(layers[0]), scene.add(layers[1])) {
                pointer = Some([below, above]);
            }
        }

        let stencils = renderer.upload_mask(DOT_SRC, &stencil(None)).is_ok()
            && [Button::Close, Button::Minimize, Button::Zoom]
                .iter()
                .zip(GLYPH_SRC)
                .all(|(button, src)| renderer.upload_mask(src, &stencil(Some(*button))).is_ok());

        let theme = crate::look::current();
        let dock = crate::dock::Dock::new(renderer, scene, &theme);

        let line_height = corrode_text::measure("> ", LINE_STYLE).height;
        let mut desk = Self {
            pointer,
            at: (screen.width as i32 / 2, screen.height as i32 / 2),
            buttons: 0,
            line: None,
            typed: String::new(),
            line_height,
            moved: false,
            stencils,
            glyphs: [None; 3],
            hovering: None,
            titles: 0,
            dock,
            theme,
            windows: Vec::new(),
            focus: None,
            drag: None,
            drag_from: (0, 0),
            snap: None,
            unsnap: false,
            menu_at: (0, 0),
            menu_above: false,
            bar_click: None,
            desktop_shown: Vec::new(),
            hovered_at: None,
            placed: 0,
            closed: Vec::new(),
            fading: Vec::new(),
            announced: None,
            can_resize: false,
            resized: Vec::new(),
            sizing: None,
            cursor: Cursor::Arrow,
            resize_cursors: resizing,
            wanted: None,
            switcher: None,
            spotlight: None,
            reveal: None,
            banner: None,
            recent: Vec::new(),
        };
        let src = Rect::new(0, LINE_ROW as i32, 1, line_height);
        let y = screen.height as i32 - 40 - line_height as i32;
        desk.line = scene.add(Layer::mask(48, y, src, LINE_COLOR));
        desk.redraw_line(renderer, scene);
        desk
    }

    /// Take the boot screen's typed line away: the desktop is up. Keys with
    /// no window to go to are still logged.
    pub fn hide_line<const N: usize>(&mut self, scene: &mut Scene<N>) {
        if let Some(line) = self.line.take() {
            scene.remove(line);
        }
    }

    /// Draw the typed line into the atlas and fit its layer to it.
    fn redraw_line<R: Renderer, const N: usize>(&mut self, renderer: &mut R, scene: &mut Scene<N>) {
        let Some(id) = self.line else { return };
        let text = alloc::format!("> {}_", self.typed);
        let extent = corrode_text::measure(&text, LINE_STYLE);
        let (width, height) = (crate::ATLAS_WIDTH, self.line_height);
        if extent.width > width || extent.height > height {
            return;
        }
        // The whole block, not just the text: the zeros after it are what
        // rubs out a longer line that was there before.
        let mut coverage = alloc::vec![0u8; (width * height) as usize];
        corrode_text::draw(
            &text,
            LINE_STYLE,
            &mut Coverage::new(&mut coverage, width as usize),
        );
        let block = Rect::new(0, LINE_ROW as i32, width, height);
        if renderer.upload_mask(block, &coverage).is_err() {
            return;
        }
        if let Some(layer) = scene.layer_mut(id) {
            layer.width = Animation::rest(Fx::from_int(extent.width as i32));
            layer.content = corrode_render::scene::Content::Mask {
                src: Rect::new(0, LINE_ROW as i32, width, height),
            };
        }
        // The pixels behind the layer changed with nothing about the layer
        // itself saying so.
        scene.damage(id, Rect::new(0, 0, width, height));
    }

    /// Take what the keyboard and mouse have done since the last frame.
    pub fn poll<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        now: u64,
    ) {
        self.restack(scene);
        self.forget_abandoned(scene, now);
        self.notifications(renderer, scene, now);

        // What programs have drawn since the last frame.
        for managed in &self.windows {
            if let Some(redrawn) = crate::surface::take_redrawn(managed.surface) {
                scene.damage(managed.body, redrawn);
            }
        }

        let mut typed = false;
        for event in drain() {
            match event {
                Event::Ready { keyboard, mouse } => {
                    crate::check("input", "the driver found a keyboard", keyboard);
                    crate::check("input", "...and a mouse", mouse);
                    if mouse {
                        self.show_pointer(scene, now);
                    }
                }
                // While the screen is covered, the cover has every key.
                Event::Key {
                    key,
                    pressed,
                    text,
                    modifiers,
                } if self.covered().is_some() => {
                    if let Some(index) = self.covered() {
                        crate::windows::push(
                            self.windows[index].window,
                            window::KEY,
                            [
                                key as u64,
                                pressed as u64,
                                text.map_or(0, |c| c as u64),
                                modifiers as u64,
                            ],
                        );
                    }
                }
                // While the switcher is up, it has the keyboard: Tab moves
                // the pick, letting go of Alt takes it, Escape takes none.
                Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if self.switcher.is_some() => match (key, pressed) {
                    (KEY_TAB, true) => self.switch(renderer, scene, modifiers & SHIFT != 0, now),
                    (KEY_LEFT_ALT | KEY_RIGHT_ALT, false) => self.end_switch(scene, true, now),
                    (KEY_ESCAPE, true) => self.end_switch(scene, false, now),
                    _ => {}
                },
                Event::Key {
                    key: KEY_TAB,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers & ALT != 0 => self.switch(renderer, scene, modifiers & SHIFT != 0, now),
                // Alt+Space: search, or put search away.
                Event::Key {
                    key: KEY_SPACE,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers & ALT != 0 => match self.spotlight.take() {
                    Some(spotlight) => spotlight.close(scene),
                    None => {
                        self.spotlight = crate::spotlight::Spotlight::open(
                            renderer,
                            scene,
                            &self.theme,
                            self.dock.as_ref(),
                            now,
                        );
                        match self.spotlight {
                            Some(_) => println!("desktop    : search up"),
                            None => println!("desktop    : no dock to search"),
                        }
                    }
                },
                // While search is up, it has the keyboard.
                Event::Key {
                    key,
                    pressed: true,
                    text,
                    modifiers,
                } if self.spotlight.is_some() && modifiers & ALT == 0 => {
                    self.search_key(renderer, scene, key, text, now)
                }
                Event::Key { .. } if self.spotlight.is_some() => {}
                // The desktop's own shortcuts come first, and go no further.
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers & ALT != 0 && Shortcut::of(key).is_some() => {
                    if let Some(shortcut) = Shortcut::of(key) {
                        self.shortcut(scene, shortcut, now);
                    }
                }
                Event::Key {
                    key,
                    pressed,
                    text,
                    modifiers,
                } => match self.focus.and_then(|index| self.windows.get(index)) {
                    // The window with the keyboard gets every key, up and
                    // down, and makes of it what it likes.
                    Some(managed) => crate::windows::push(
                        managed.window,
                        window::KEY,
                        [
                            key as u64,
                            pressed as u64,
                            text.map_or(0, |c| c as u64),
                            modifiers as u64,
                        ],
                    ),
                    None => {
                        if let (true, Some(text)) = (pressed, text) {
                            typed |= self.key(text);
                        }
                    }
                },
                Event::Pointer {
                    dx,
                    dy,
                    buttons,
                    wheel,
                } => {
                    self.pointer(scene, now, dx, dy, buttons);
                    if wheel != 0 {
                        self.scroll(scene, wheel);
                    }
                    self.restack(scene);
                }
            }
        }
        if typed {
            self.redraw_line(renderer, scene);
        }
        self.announce();

        // An edge dragged: once a frame, however many moves it took.
        if let Some((index, size, (right, bottom))) = self.wanted.take() {
            let current = self.windows.get(index).map(|managed| (managed.width, managed.height));
            if current.is_some_and(|current| current != size) {
                self.resize(scene, index, size);
                let managed = &mut self.windows[index];
                // The far edges stay; the window moves by what it grew.
                if let Some(right) = right {
                    managed.at.0 = right - managed.width as i32;
                }
                if let Some(bottom) = bottom {
                    managed.at.1 = bottom - (managed.bar() + managed.height) as i32;
                }
                // Its own size now, whatever zoom said before.
                managed.unzoomed = None;
                for (id, (x, y)) in managed.places(managed.at, self.theme.buttons) {
                    if let Some(layer) = scene.layer_mut(id) {
                        layer.x = Animation::rest(Fx::from_int(x));
                        layer.y = Animation::rest(Fx::from_int(y));
                    }
                }
                self.hide_glyphs(scene);
            }
        }
    }

    /// Tell docked windows -- the menu bar -- which app is in front, when
    /// that has changed since they were last told.
    /// The window covering the screen, if one is.
    fn covered(&self) -> Option<usize> {
        self.windows.iter().rposition(|managed| managed.cover)
    }

    fn announce(&mut self) {
        // A popover is the menu bar's, not a program in front of it.
        let front = self
            .focus
            .and_then(|index| self.windows.get(index))
            .filter(|managed| !managed.popover && !managed.cover)
            .map(|managed| managed.name.clone())
            .unwrap_or_default();
        if self.announced.as_ref() == Some(&front) {
            return;
        }
        let mut title = [0u8; 8];
        for (slot, byte) in title.iter_mut().zip(front.bytes()) {
            *slot = byte;
        }
        for managed in self.windows.iter().filter(|managed| managed.docked) {
            crate::windows::push(
                managed.window,
                window::ACTIVE,
                [u64::from_le_bytes(title), 0, 0, 0],
            );
        }
        self.announced = Some(front);
    }

    /// A character typed. True if the line changed.
    fn key(&mut self, text: char) -> bool {
        match text {
            '\u{8}' => self.typed.pop().is_some(),
            '\n' => {
                println!("input      : line {:?}", self.typed);
                self.typed.clear();
                true
            }
            ' '..='~' if self.typed.len() < LINE_MAX => {
                self.typed.push(text);
                true
            }
            _ => false,
        }
    }

    /// Draw a window's title into a free slot of the atlas. The slot, and
    /// how wide the title came out.
    fn draw_title<R: Renderer>(&mut self, renderer: &mut R, title: &str) -> Option<(usize, u32)> {
        self.draw_text(renderer, title, TITLE_STYLE)
    }

    /// Draw `text` into a free title slot: the slot, and how wide the text
    /// came out. None if it is empty, will not fit, or no slot is free.
    fn draw_text<R: Renderer>(&mut self, renderer: &mut R, text: &str, style: Style) -> Option<(usize, u32)> {
        if text.is_empty() {
            return None;
        }
        let slot = (0..TITLE_SLOTS).find(|slot| self.titles & 1 << slot == 0)?;
        let (width, height) = TITLE_SLOT;
        let extent = corrode_text::measure(text, style);
        if extent.width > width || extent.height > height {
            return None;
        }
        let mut coverage = alloc::vec![0u8; (width * height) as usize];
        corrode_text::draw(text, style, &mut Coverage::new(&mut coverage, width as usize));
        renderer.upload_mask(Self::title_src(slot, width), &coverage).ok()?;
        self.titles |= 1 << slot;
        Some((slot, extent.width))
    }

    /// Draw a window's title bar text again, after its program changed its
    /// subtitle. The new text goes in a free slot before the old one is
    /// given back, so a full atlas keeps the old title rather than none.
    pub fn retitle<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        window: usize,
        scene: &mut Scene<N>,
        now: u64,
    ) {
        let Some(index) = self.windows.iter().position(|managed| managed.window == window) else {
            return;
        };
        let Some((id, old, _)) = self.windows[index].title else {
            return;
        };
        let caption = crate::windows::caption(window);
        // Too long for the bar: the title alone.
        let fitted = self
            .draw_title(renderer, &caption)
            .filter(|(_, width)| *width + 2 * 80 <= self.windows[index].width)
            .or_else(|| self.draw_title(renderer, &crate::windows::title(window)));
        let Some((slot, width)) = fitted else {
            return;
        };
        self.titles &= !(1 << old);
        if let Some(layer) = scene.layer_mut(id) {
            let src = Self::title_src(slot, width);
            layer.content = corrode_render::scene::Content::Mask { src };
            layer.width = Animation::rest(Fx::from_int(src.width as i32));
        }
        let managed = &mut self.windows[index];
        managed.title = Some((id, slot, width));
        if !managed.minimized {
            // Only the title moves, to stay centred at its new width. The
            // rest of the window may be zooming in: the title goes on to
            // its new place from wherever it is, rather than jumping there.
            let place = managed.places(managed.at, self.theme.buttons).into_iter().find(|(at, _)| *at == id);
            if let (Some((_, (x, y))), Some(layer)) = (place, scene.layer_mut(id)) {
                let moving = layer.x.end_ns() > now || layer.y.end_ns() > now;
                let to = |animation: &Animation, place: i32| match moving {
                    true => animation.retarget(now, Fx::from_int(place), Spring::SMOOTH),
                    false => Animation::rest(Fx::from_int(place)),
                };
                layer.x = to(&layer.x, x);
                layer.y = to(&layer.y, y);
            }
        }
        println!("desktop    : window {} now says {}", window, caption);
    }

    /// Bring `window` to the front, as picking it in a dock menu does.
    /// A popover asked to be `size`: Control Center, as notifications come
    /// and go. It keeps its place; only its height and width change.
    pub fn grow<const N: usize>(&mut self, window: usize, scene: &mut Scene<N>, size: (u32, u32)) {
        if !self.can_resize {
            return;
        }
        if let Some(index) = self
            .windows
            .iter()
            .position(|managed| managed.window == window && managed.popover)
        {
            self.resize(scene, index, size);
        }
    }

    pub fn raise<const N: usize>(&mut self, window: usize, scene: &mut Scene<N>, now: u64) {
        if let Some(index) = self.windows.iter().position(|managed| managed.window == window) {
            println!("desktop    : brought window {} forward", window);
            self.restore(scene, index, now);
        }
    }

    fn title_src(slot: usize, width: u32) -> Rect {
        let (slot_width, slot_height) = TITLE_SLOT;
        let (row, slot) = if slot < TITLE_SLOTS_ABOVE {
            (TITLE_ROW, slot)
        } else {
            (TITLE_ROW_BELOW, slot - TITLE_SLOTS_ABOVE)
        };
        Rect::new(
            (slot % 2) as i32 * slot_width as i32,
            row as i32 + (slot / 2) as i32 * slot_height as i32,
            width.min(slot_width),
            slot_height,
        )
    }

    /// Give the painter a window the server has made: a place, a frame of
    /// glass with buttons and a title, a shadow, and the keyboard. It rises
    /// into place and fades in, like everything else that arrives.
    pub fn manage<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        window: usize,
        scene: &mut Scene<N>,
        now: u64,
    ) {
        let Some((surface, width, height, docked)) = crate::windows::shape(window) else {
            return;
        };
        let theme = self.theme;
        let popover = crate::windows::popover(window);
        let cover = crate::windows::cover(window);
        let chrome = !docked && !popover && !cover;
        let at = if docked || cover {
            (0, 0)
        } else if popover && crate::windows::menu(window) {
            // A context menu: its corner at the pointer, or above a dock
            // icon and centred on it; kept on screen.
            let screen = scene.screen();
            let (x, y) = if self.menu_above {
                (self.menu_at.0 - width as i32 / 2, self.menu_at.1 - height as i32)
            } else {
                self.menu_at
            };
            (
                x.min(screen.width as i32 - width as i32 - 4).max(4),
                y.min(screen.height as i32 - height as i32 - 4).max(4),
            )
        } else if popover {
            // Under the right-hand end of the menu bar, as Control Center
            // hangs from it.
            (scene.screen().width as i32 - width as i32 - 8, POPOVER_TOP)
        } else {
            let step = self.placed % 6;
            self.placed += 1;
            (120 + 40 * step, 90 + 36 * step)
        };
        let radius = if docked || cover { 0 } else { theme.radius as u16 };
        // A context menu takes down the dock's label, which could be over it.
        if popover && crate::windows::menu(window) {
            if let Some(dock) = &mut self.dock {
                dock.hover(scene, None, now);
            }
        }
        // A popover hangs where banners slide in: it sends any away, as
        // opening Notification Center does on macOS.
        if popover {
            if let Some(banner) = &mut self.banner {
                banner.dismiss(scene, &theme, now);
            }
        }
        // Nothing else is to be used while the screen is covered.
        if cover {
            if let Some(spotlight) = self.spotlight.take() {
                spotlight.close(scene);
            }
            if let Some(switcher) = self.switcher.take() {
                switcher.close(scene);
            }
            println!("desktop    : screen covered");
        }
        let translucent = crate::windows::translucent(window);

        // Bottom first, so each lands above the last. The shadow is there
        // even with shadows off, unseen, so turning them on can show it in
        // its place under the window.
        let shadow = (!docked && !cover)
            .then(|| {
                let bar = if popover { 0 } else { BAR };
                scene.add(Layer::shadow(0, 0, width, height + bar, radius, SHADOW_BLUR))
            })
            .flatten();
        // A window fades in as it opens, all of its frame there meanwhile.
        let fades_in = chrome && theme.ms(250) > 0;
        let frame_height = frame_height(height, if chrome { BAR } else { 0 }, translucent || fades_in, radius as u32);
        let frame = scene.add(pane(Rect::new(0, 0, width, frame_height), radius, &theme));
        let mut lights = [None; 3];
        if chrome && self.stencils {
            for (slot, button) in lights.iter_mut().zip(Button::order(theme.buttons)) {
                *slot = scene
                    .add(Layer::mask(0, 0, DOT_SRC, button.color()))
                    .map(|id| (button, id));
            }
        }
        let name = crate::windows::title(window);
        let title = if !chrome {
            None
        } else {
            self.draw_title(renderer, &crate::windows::caption(window))
                .and_then(|(slot, text_width)| {
                    let src = Self::title_src(slot, text_width);
                    scene
                        .add(Layer::mask(0, 0, src, theme.text()))
                        .map(|id| (id, slot, text_width))
                })
        };
        let mut body = if translucent {
            Layer::translucent_surface(0, 0, surface as u32, width, height)
        } else {
            Layer::surface(0, 0, surface as u32, width, height)
        };
        if let corrode_render::scene::Content::Surface { radius: corner, .. } = &mut body.content {
            *corner = radius.min(255) as u8;
        }
        let Some(body) = scene.add(body) else {
            println!("desktop    : no room in the scene for another window");
            for id in shadow
                .into_iter()
                .chain(frame)
                .chain(lights.iter().flatten().map(|(_, id)| *id))
                .chain(title.map(|(id, ..)| id))
            {
                scene.remove(id);
            }
            if let Some((_, slot, _)) = title {
                self.titles &= !(1 << slot);
            }
            return;
        };

        let managed = Managed {
            window,
            surface,
            docked,
            popover,
            cover,
            at,
            width,
            height,
            shadow,
            frame,
            lights,
            title,
            name,
            minimized: false,
            unzoomed: None,
            body,
            fade_until: fades_in.then(|| now + theme.ms(250) as u64 * 1_000_000),
        };
        let rise = theme.ms(250);
        // A window zooms in from a little smaller, about its own middle, as
        // a macOS window opens: every layer of it from the same point, on
        // the same curve, so they stay together. Glass, shadow and body grow
        // with it, the body's surface stretched meanwhile; the buttons and
        // the title, cut from the atlas at their own size, only move.
        let middle = (
            at.0 + width as i32 / 2,
            at.1 + (managed.bar() + height) as i32 / 2,
        );
        let zoom_in = |place: i32, size: i32, middle: i32, grows: bool| -> (i32, i32) {
            let (scaled, from_size) = match grows {
                true => (middle + (place - middle) * ZOOM_FROM / 100, size * ZOOM_FROM / 100),
                false => (middle + (place + size / 2 - middle) * ZOOM_FROM / 100 - size / 2, size),
            };
            (scaled, from_size)
        };
        if chrome && rise > 0 {
            println!("desktop    : window {} zooms in from {}%", window, ZOOM_FROM);
        }
        for (id, (x, y)) in managed.places(at, theme.buttons) {
            if let Some(layer) = scene.layer_mut(id) {
                layer.x = Animation::rest(Fx::from_int(x));
                layer.y = Animation::rest(Fx::from_int(y));
                if chrome && rise > 0 {
                    layer.opacity =
                        Animation::ease(now, rise, Fx::ZERO, Fx::from_int(255), Ease::Out);
                    use corrode_render::scene::Content;
                    let grows = !matches!(layer.content, Content::Mask { .. } | Content::ScaledMask { .. });
                    if let Content::Surface { stretch, .. } = &mut layer.content {
                        *stretch = true;
                    }
                    let (w, h) = (layer.width.target().round(), layer.height.target().round());
                    let (from_x, from_w) = zoom_in(x, w, middle.0, grows);
                    let (from_y, from_h) = zoom_in(y, h, middle.1, grows);
                    let ease = |from: i32, to: i32| Animation::ease(now, rise, Fx::from_int(from), Fx::from_int(to), Ease::Out);
                    layer.x = ease(from_x, x);
                    layer.y = ease(from_y, y);
                    layer.width = ease(from_w, w);
                    layer.height = ease(from_h, h);
                    continue;
                }
                if !docked && rise > 0 {
                    layer.opacity =
                        Animation::ease(now, rise, Fx::ZERO, Fx::from_int(255), Ease::Out);
                    // A popover drops from the menu bar; a cover fades in
                    // where it is; a window rises.
                    let from = if popover {
                        y - 10
                    } else if cover {
                        y
                    } else {
                        y + 24
                    };
                    layer.y = Animation::spring(
                        now,
                        Fx::from_int(from),
                        Fx::from_int(y),
                        corrode_render::anim::Velocity::ZERO,
                        corrode_render::anim::Spring::SMOOTH,
                    );
                }
            }
        }
        let is_files = !docked && managed.name == "Files";
        self.windows.push(managed);
        println!("desktop    : window {} opened, {} open", window, self.windows.len());
        if let (true, Some(words)) = (is_files, self.reveal) {
            self.reveal = None;
            crate::windows::push(window, window::REVEAL, words);
        }
        if docked || cover {
            self.announced = None;
            if let Some((unix, at)) = crate::rtc::anchor() {
                crate::windows::push(window, window::CLOCK, [unix as u64, at, 0, 0]);
            }
        }
        self.show_running(scene);
        crate::windows::push(window, window::THEME, theme.to_words());
        let index = self.windows.len() - 1;
        self.paint_focus(scene, index, false);
        if !docked {
            self.set_focus(scene, Some(index));
        }
    }

    /// Show window `index` as having the keyboard or not: a tint of the
    /// accent in its glass, coloured buttons, a bright title and a deeper
    /// shadow -- or a plain pane, grey buttons, a dim title, a light shadow.
    fn paint_focus<const N: usize>(&self, scene: &mut Scene<N>, index: usize, focused: bool) {
        let Some(managed) = self.windows.get(index) else {
            return;
        };
        // A window put away stays unseen until it is brought back.
        if managed.docked || managed.minimized {
            return;
        }
        let theme = &self.theme;
        if let Some(layer) = managed.frame.and_then(|id| scene.layer_mut(id)) {
            if let corrode_render::scene::Content::Glass(glass) = &mut layer.content {
                glass.tint = theme.accent;
                glass.tint_amount = if focused { 40 } else { theme.tint_amount };
                if !focused && theme.tint_amount > 0 {
                    glass.tint = theme.tint;
                }
            }
        }
        let idle = if theme.dark {
            Color::rgb(0x55, 0x56, 0x5c)
        } else {
            Color::rgb(0xc8, 0xc8, 0xcc)
        };
        for (button, id) in managed.lights.iter().flatten() {
            if let Some(layer) = scene.layer_mut(*id) {
                layer.color = if focused { button.color() } else { idle };
            }
        }
        if let Some(layer) = managed.title.and_then(|(id, ..)| scene.layer_mut(id)) {
            layer.opacity = Animation::rest(Fx::from_int(if focused { 255 } else { 140 }));
        }
        if let Some(layer) = managed.shadow.and_then(|id| scene.layer_mut(id)) {
            // Lighter on a light desktop, where a dark shadow is a smudge.
            let opacity = match (theme.shadows, focused, theme.dark) {
                (false, ..) => 0,
                (true, true, true) => 120,
                (true, false, true) => 70,
                (true, true, false) => 64,
                (true, false, false) => 36,
            };
            layer.opacity = Animation::rest(Fx::from_int(opacity));
        }
    }

    /// Give the keyboard to window `index`, or to nobody. The window that
    /// gets it comes to the front; both are told.
    fn set_focus<const N: usize>(&mut self, scene: &mut Scene<N>, index: Option<usize>) {
        if self.focus == index {
            return;
        }
        for (which, gained) in [(self.focus, false), (index, true)] {
            let Some(which) = which else {
                continue;
            };
            let Some(managed) = self.windows.get(which) else {
                continue;
            };
            crate::windows::push(managed.window, window::FOCUS, [gained as u64, 0, 0, 0]);
            // A popover lasts as long as it has the keyboard.
            if !gained && managed.popover {
                crate::windows::push(managed.window, window::CLOSE, [0; 4]);
            }
            if gained && !managed.docked {
                let id = managed.window;
                self.recent.retain(|recent| *recent != id);
                self.recent.insert(0, id);
            }
            if gained {
                let ids: Vec<LayerId> = managed.layers().collect();
                for id in ids {
                    scene.raise(id);
                }
            }
            self.paint_focus(scene, which, gained);
        }
        self.focus = index;
        self.restack(scene);
    }

    /// The dock and docked windows above every other window, the button
    /// glyphs above them, and the pointer above all.
    fn restack<const N: usize>(&self, scene: &mut Scene<N>) {
        let wanted = self.pointer.map(|layers| layers[1]);
        if wanted.is_some() && scene.top() == wanted {
            return;
        }
        if let Some(dock) = &self.dock {
            let ids: Vec<LayerId> = dock.layers().collect();
            for id in ids {
                scene.raise(id);
            }
        }
        for managed in self.windows.iter().filter(|managed| managed.docked) {
            for id in managed.layers() {
                scene.raise(id);
            }
        }
        for glyph in self.glyphs.iter().flatten() {
            scene.raise(*glyph);
        }
        if let Some(switcher) = &self.switcher {
            let ids: Vec<LayerId> = switcher.layers().collect();
            for id in ids {
                scene.raise(id);
            }
        }
        if let Some(spotlight) = &self.spotlight {
            let ids: Vec<LayerId> = spotlight.layers().collect();
            for id in ids {
                scene.raise(id);
            }
        }
        if let Some(banner) = &self.banner {
            let ids: Vec<LayerId> = banner.layers().collect();
            for id in ids {
                scene.raise(id);
            }
        }
        // A cover goes over everything but the pointer, fading or not.
        for fading in self.fading.iter().filter(|fading| fading.on_top) {
            for id in &fading.layers {
                scene.raise(*id);
            }
        }
        for managed in self.windows.iter().filter(|managed| managed.cover) {
            for id in managed.layers() {
                scene.raise(id);
            }
        }
        if let Some([below, above]) = self.pointer {
            scene.raise(below);
            scene.raise(above);
        }
    }

    /// What is under the pointer: the window and which part of it, looking
    /// past the pointer, the glyphs and shadows, which are never what was
    /// meant. `Err` with whether anything else at all is there.
    fn under<const N: usize>(&self, scene: &Scene<N>) -> Result<(usize, Part), bool> {
        let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
        let mut any = false;
        for id in scene.layers_at(self.at.0, self.at.1) {
            if pointer.contains(&id) || self.glyphs.contains(&Some(id)) {
                continue;
            }
            let found = self
                .windows
                .iter()
                .enumerate()
                .find_map(|(index, managed)| managed.part(id).map(|part| (index, part)));
            match found {
                Some((_, Part::Shadow)) => continue,
                Some(hit) => return Ok(hit),
                None => {
                    any = true;
                    break;
                }
            }
        }
        Err(any)
    }

    /// A button went down or up. What is under the pointer decides what it
    /// means: a bar is picked up, a window button pressed, a window body
    /// given the keyboard and told, and anything else takes the keyboard
    /// away from every window.
    fn click<const N: usize>(&mut self, scene: &mut Scene<N>, buttons: u8, pressed: u8, now: u64) {
        // A click anywhere but on an open popover closes it. On the menu
        // bar it does nothing more, so clicking the clock again shuts
        // Control Center rather than opening it anew.
        if pressed & 3 != 0 {
            let open: Vec<usize> = (0..self.windows.len())
                .filter(|index| self.windows[*index].popover)
                .collect();
            if !open.is_empty() {
                let hit = self.under(scene).ok().map(|(index, _)| index);
                if hit.is_none_or(|hit| !open.contains(&hit)) {
                    for index in open {
                        crate::windows::push(self.windows[index].window, window::CLOSE, [0; 4]);
                    }
                    println!("input      : popover closed by a click outside it");
                    if hit.is_some_and(|hit| self.windows[hit].docked) {
                        return;
                    }
                }
            }
        }
        // The right button on the bare desktop: its menu, at the pointer.
        if pressed & 2 != 0 && self.covered().is_none() && self.dock_menu(scene) {
            return;
        }
        if pressed & 2 != 0 && self.covered().is_none() && self.on_desktop(scene) {
            self.menu_at = self.at;
            self.menu_above = false;
            crate::context::set([crate::context::DESKTOP, 0, 0, 0]);
            println!("input      : right click on the desktop at ({}, {})", self.at.0, self.at.1);
            launch(launcher::MENU);
            return;
        }
        // The dock first: it is above every window.
        if pressed & 1 != 0 {
            let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
            let top = scene
                .layers_at(self.at.0, self.at.1)
                .find(|id| !pointer.contains(id));
            // A notification clicked goes away, and nothing under it hears.
            if let (Some(banner), Some(top)) = (&mut self.banner, top) {
                if banner.contains(top) {
                    let theme = self.theme;
                    banner.dismiss(scene, &theme, now);
                    println!("input      : notification dismissed");
                    return;
                }
            }
            if let (Some(dock), Some(top)) = (&self.dock, top) {
                if let Some(icon) = dock.icon(top) {
                    println!("input      : click at ({}, {}) on the dock", self.at.0, self.at.1);
                    // A program with a window already comes forward, out of
                    // the dock if it was put away; one without is started.
                    let title = dock.title(icon);
                    let open = self
                        .windows
                        .iter()
                        .rposition(|managed| !managed.docked && managed.name == title);
                    match open {
                        Some(index) => self.restore(scene, index, now),
                        None => dock.launch(icon, scene, now),
                    }
                    return;
                }
                if dock.contains(top) {
                    return;
                }
            }
        }
        let hit = self.under(scene);

        if pressed & 1 != 0 {
            println!(
                "input      : click at ({}, {}) on {}",
                self.at.0,
                self.at.1,
                match hit {
                    Ok((_, Part::Light(Button::Close))) => "a close button",
                    Ok((_, Part::Light(Button::Minimize))) => "a minimize button",
                    Ok((_, Part::Light(Button::Zoom))) => "a zoom button",
                    Ok((_, Part::Bar)) => "a title bar",
                    Ok(_) => "a window",
                    Err(true) => "a layer",
                    Err(false) => "the background",
                }
            );
            match hit {
                Ok((index, part)) => {
                    if !self.windows[index].docked {
                        self.set_focus(scene, Some(index));
                    }
                    match part {
                        // Asked, not done: the window goes when its program
                        // does.
                        Part::Light(Button::Close) => {
                            crate::windows::push(self.windows[index].window, window::CLOSE, [0; 4])
                        }
                        Part::Light(Button::Minimize) => self.minimize(scene, index, now),
                        Part::Light(Button::Zoom) => self.zoom(scene, index, now),
                        // A double click on the bar zooms, as on macOS.
                        Part::Bar
                            if self.bar_click.is_some_and(|(window, at, (x, y))| {
                                window == self.windows[index].window
                                    && now.saturating_sub(at) < DOUBLE_CLICK_NS
                                    && (self.at.0 - x).abs() <= 4
                                    && (self.at.1 - y).abs() <= 4
                            }) =>
                        {
                            self.bar_click = None;
                            println!("desktop    : double click on a title bar");
                            self.zoom(scene, index, now);
                        }
                        Part::Bar => {
                            self.bar_click = Some((self.windows[index].window, now, self.at));
                            let (x, y) = self.windows[index].at;
                            self.drag = Some((index, self.at.0 - x, self.at.1 - y));
                            self.drag_from = (x, y);
                            self.unsnap = self.windows[index].unzoomed.is_some();
                        }
                        Part::Body | Part::Shadow => {}
                    }
                }
                Err(_) => self.set_focus(scene, None),
            }
        }

        // Pressed near an edge of a resizable window: the edge is picked
        // up, and the program hears nothing of it.
        if let (Ok((index, part)), true) = (hit, pressed & 1 != 0) {
            let edges = self.edges(index, part);
            if edges.0 || edges.1 || edges.2 || edges.3 {
                let managed = &self.windows[index];
                self.drag = None;
                self.sizing = Some(Sizing {
                    index,
                    from: self.at,
                    size: (managed.width, managed.height),
                    left: edges.0,
                    top: edges.1,
                    right: edges.2,
                    bottom: edges.3,
                });
                return;
            }
        }
        if self.sizing.is_some() {
            if buttons & 1 == 0 {
                self.sizing = None;
            }
            return;
        }

        // A click on a window's body is the program's to interpret.
        if let Ok((index, Part::Body)) = hit {
            let managed = &self.windows[index];
            let (x, y) = (managed.at.0, managed.at.1 + managed.bar() as i32);
            crate::windows::push(
                managed.window,
                window::BUTTON,
                [
                    (self.at.0 - x) as u64,
                    (self.at.1 - y) as u64,
                    buttons as u64,
                    0,
                ],
            );
        }
    }

    /// The wheel turned: tell the window under the pointer, wherever on it
    /// the pointer is, as macOS scrolls what is under the pointer and not
    /// what has focus.
    fn scroll<const N: usize>(&mut self, scene: &Scene<N>, notches: i32) {
        let Ok((index, Part::Body)) = self.under(scene) else {
            return;
        };
        let managed = &self.windows[index];
        let (x, y) = (managed.at.0, managed.at.1 + managed.bar() as i32);
        crate::windows::push(
            managed.window,
            window::SCROLL,
            [
                notches as i64 as u64,
                (self.at.0 - x) as u64,
                (self.at.1 - y) as u64,
                0,
            ],
        );
    }

    /// Show the glyphs on the buttons of the window the pointer is over the
    /// buttons of, as macOS does: the dots say nothing until you reach for
    /// them.
    fn hover<const N: usize>(&mut self, scene: &mut Scene<N>) {
        let over = match self.under(scene) {
            Ok((index, Part::Light(_))) => Some(index),
            _ => None,
        };
        if over == self.hovering {
            return;
        }
        self.hovering = over;
        let theme = self.theme;
        for (slot, src) in GLYPH_SRC.iter().enumerate() {
            let button = [Button::Close, Button::Minimize, Button::Zoom][slot];
            let place = over.and_then(|index| {
                let managed = &self.windows[index];
                let position = managed.lights.iter().position(|light| {
                    light.is_some_and(|(which, _)| which == button)
                })?;
                Some(managed.light_at(managed.at, position, theme.buttons))
            });
            match (place, self.glyphs[slot]) {
                (Some((x, y)), Some(id)) => {
                    if let Some(layer) = scene.layer_mut(id) {
                        layer.x = Animation::rest(Fx::from_int(x));
                        layer.y = Animation::rest(Fx::from_int(y));
                        layer.opacity = Animation::rest(Fx::from_int(255));
                    }
                }
                (Some((x, y)), None) => {
                    self.glyphs[slot] = scene.add(Layer::mask(x, y, *src, GLYPH_COLOR));
                }
                (None, Some(id)) => {
                    if let Some(layer) = scene.layer_mut(id) {
                        layer.opacity = Animation::rest(Fx::ZERO);
                    }
                }
                (None, None) => {}
            }
        }
        self.restack(scene);
    }

    /// Take away the windows whose programs have finished: exited, perhaps
    /// because their close button was clicked, or stopped for a fault.
    fn forget_abandoned<const N: usize>(&mut self, scene: &mut Scene<N>, now: u64) {
        // Fades that have run their course.
        let mut kept = Vec::new();
        for fading in core::mem::take(&mut self.fading) {
            if now < fading.until {
                kept.push(fading);
                continue;
            }
            for id in fading.layers {
                scene.remove(id);
            }
            if let Some(slot) = fading.slot {
                self.titles &= !(1 << slot);
            }
            self.closed.push(fading.surface);
        }
        self.fading = kept;
        // Frames given all of a window for a fade, back to the bar and a
        // radius once it is over.
        let radius = self.theme.radius as u32;
        for managed in &mut self.windows {
            if managed.fade_until.is_some_and(|until| now >= until) {
                managed.fade_until = None;
                let rows = managed.frame_rows(if managed.docked { 0 } else { radius });
                if let Some(layer) = managed.frame.and_then(|id| scene.layer_mut(id)) {
                    layer.height = Animation::rest(Fx::from_int(rows as i32));
                }
            }
        }

        let mut index = 0;
        while index < self.windows.len() {
            if !crate::windows::abandoned(self.windows[index].window) {
                index += 1;
                continue;
            }
            let mut managed = self.windows.remove(index);
            // A window fades out where it was; a docked one just goes.
            let ms = if managed.docked { 0 } else { self.theme.ms(CLOSING_MS) };
            let layers: Vec<LayerId> = managed.layers().collect();
            if ms > 0 {
                managed.fade_frame(scene, now + ms as u64 * 1_000_000);
                for id in &layers {
                    if let Some(layer) = scene.layer_mut(*id) {
                        let from = layer.opacity.sample(now).value;
                        layer.opacity = Animation::ease(now, ms, from, Fx::ZERO, Ease::Out);
                    }
                }
            }
            self.fading.push(Fading {
                layers,
                surface: managed.surface,
                slot: managed.title.map(|(_, slot, _)| slot),
                until: now + ms as u64 * 1_000_000,
                on_top: managed.cover,
            });
            println!("desktop    : window {} closed", managed.window);
            self.recent.retain(|recent| *recent != managed.window);
            // The switcher's list is out of date: put it away.
            if let Some(switcher) = self.switcher.take() {
                switcher.close(scene);
            }
            self.show_running(scene);
            // Indices past this one have moved down by one.
            let shift = |at: usize| match at.cmp(&index) {
                core::cmp::Ordering::Less => Some(at),
                core::cmp::Ordering::Equal => None,
                core::cmp::Ordering::Greater => Some(at - 1),
            };
            self.focus = self.focus.and_then(shift);
            self.drag = self
                .drag
                .and_then(|(at, x, y)| shift(at).map(|at| (at, x, y)));
            self.hide_glyphs(scene);
        }
    }

    /// Alt+Tab: put the switcher up with the window behind the front one
    /// picked, or, if it is up, pick the next one (`back`: the one before).
    fn switch<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        back: bool,
        now: u64,
    ) {
        if let Some(switcher) = &mut self.switcher {
            switcher.step(scene, back, now);
            return;
        }
        // Most recently in front first, then any never in front.
        let mut order: Vec<usize> = self
            .recent
            .iter()
            .filter_map(|id| self.windows.iter().position(|managed| managed.window == *id))
            .filter(|index| {
                let managed = &self.windows[*index];
                !managed.docked && !managed.popover && !managed.cover
            })
            .collect();
        for (index, managed) in self.windows.iter().enumerate() {
            if !managed.docked && !managed.popover && !managed.cover && !order.contains(&index) {
                order.push(index);
            }
        }
        // The window in front first, whatever happened since it came there,
        // and minimized ones last, as macOS orders them.
        if let Some(front) = self.focus.filter(|front| order.contains(front)) {
            order.retain(|index| *index != front);
            order.insert(0, front);
        }
        let windows = &self.windows;
        order.sort_by_key(|index| windows[*index].minimized);
        if order.len() < 2 {
            // Nothing to choose between: a lone window comes forward.
            if let Some(&index) = order.first() {
                self.restore(scene, index, now);
            }
            return;
        }
        let size = self.dock.as_ref().map_or(48, |dock| dock.size());
        let entries = order
            .iter()
            .map(|&index| {
                let managed = &self.windows[index];
                crate::switcher::Entry {
                    window: managed.window,
                    icon: self.dock.as_ref().and_then(|dock| {
                        dock.find(&managed.name).and_then(|app| dock.stencils(app))
                    }),
                    title: managed.title.and_then(|(id, _, _)| match scene.layer(id)?.content {
                        corrode_render::scene::Content::Mask { src } => Some(src),
                        _ => None,
                    }),
                }
            })
            .collect();
        let picked = if back { order.len() - 1 } else { 1 };
        self.switcher =
            crate::switcher::Switcher::open(renderer, scene, &self.theme, size, entries, picked, now);
        match &self.switcher {
            Some(_) => println!("desktop    : switcher up, {} windows", order.len()),
            // No room in the scene for it: just go to the pick.
            None => self.restore(scene, order[picked], now),
        }
    }

    /// Notifications: take down the banner once it has gone, and put up
    /// the next one waiting.
    fn notifications<R: Renderer, const N: usize>(&mut self, renderer: &mut R, scene: &mut Scene<N>, now: u64) {
        let theme = self.theme;
        if let Some(banner) = &mut self.banner {
            if !banner.tick(scene, &theme, now) {
                return;
            }
            if let Some(banner) = self.banner.take() {
                for slot in banner.slots {
                    self.titles &= !(1 << slot);
                }
                banner.close(scene);
                println!("desktop    : notification gone");
            }
        }
        // Under an open popover they would cover it: they wait.
        if self.windows.iter().any(|managed| managed.popover) {
            return;
        }
        let Some((from, text)) = crate::notify::take() else {
            return;
        };
        let body = Style::px(13);
        let (Some((name_slot, name_width)), text) = (
            self.draw_text(renderer, &from, TITLE_STYLE),
            // Too wide for its slot: cut, a character at a time.
            {
                let mut text = text;
                while !text.is_empty() && corrode_text::measure(&text, body).width > TITLE_SLOT.0 {
                    text.pop();
                }
                text
            },
        ) else {
            println!("desktop    : no room to show a notification");
            return;
        };
        let Some((text_slot, text_width)) = self.draw_text(renderer, &text, body) else {
            self.titles &= !(1 << name_slot);
            println!("desktop    : no room to show a notification");
            return;
        };
        let icon = self
            .dock
            .as_ref()
            .and_then(|dock| dock.find(&from).and_then(|app| dock.stencils(app)));
        let content = crate::banner::Content {
            icon,
            name: (Self::title_src(name_slot, TITLE_SLOT.0), name_width),
            text: (Self::title_src(text_slot, TITLE_SLOT.0), text_width),
            slots: [name_slot, text_slot],
        };
        self.banner = crate::banner::Banner::show(scene, &theme, content, now);
        match self.banner {
            Some(_) => println!("desktop    : notification from {} shown", from),
            None => {
                self.titles &= !(1 << name_slot | 1 << text_slot);
            }
        }
    }

    /// Show the file at `path` in Files: told to its window if it is open,
    /// or kept for the window of a Files started for it.
    fn reveal_file<const N: usize>(&mut self, scene: &mut Scene<N>, path: &str, now: u64) {
        let bytes = path.as_bytes();
        if bytes.len() > 32 {
            println!("desktop    : {} is too long a path to send", path);
            return;
        }
        let mut words = [0u64; 4];
        for (index, byte) in bytes.iter().enumerate() {
            words[index / 8] |= (*byte as u64) << (index % 8 * 8);
        }
        let open = self
            .windows
            .iter()
            .rposition(|managed| !managed.docked && managed.name == "Files");
        match (open, &self.dock) {
            (Some(index), _) => {
                crate::windows::push(self.windows[index].window, window::REVEAL, words);
                self.restore(scene, index, now);
            }
            (None, Some(dock)) => match dock.find("Files") {
                Some(app) => {
                    self.reveal = Some(words);
                    dock.launch(app, scene, now);
                }
                None => println!("desktop    : no Files to show {} in", path),
            },
            (None, None) => println!("desktop    : no dock to start Files from"),
        }
    }

    /// A key pressed while search is up.
    fn search_key<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        key: u8,
        text: Option<char>,
        now: u64,
    ) {
        let Some(spotlight) = &mut self.spotlight else { return };
        match key {
            KEY_ESCAPE => {
                if let Some(spotlight) = self.spotlight.take() {
                    spotlight.close(scene);
                }
            }
            KEY_UP | KEY_DOWN => spotlight.pick(scene, key == KEY_UP),
            KEY_BACKSPACE => spotlight.typed(renderer, scene, self.dock.as_ref(), None),
            KEY_ENTER => {
                let (picked, query) = (spotlight.picked(), String::from(spotlight.query()));
                if let Some(spotlight) = self.spotlight.take() {
                    spotlight.close(scene);
                }
                let (app, dock) = match (picked, &self.dock) {
                    (Some(crate::spotlight::Hit::App(app)), Some(dock)) => (app, dock),
                    (Some(crate::spotlight::Hit::File(path)), _) => {
                        println!("desktop    : search {:?} shows {}", query, path);
                        self.reveal_file(scene, &path, now);
                        return;
                    }
                    _ => {
                        println!("desktop    : nothing matches {:?}", query);
                        return;
                    }
                };
                let title = dock.title(app);
                println!("desktop    : search {:?} opens {}", query, title);
                // Already open: its window comes forward, as Spotlight
                // brings a running app forward rather than starting another.
                let open = self
                    .windows
                    .iter()
                    .rposition(|managed| !managed.docked && managed.name == title);
                match open {
                    Some(index) => self.restore(scene, index, now),
                    None => dock.launch(app, scene, now),
                }
            }
            _ => {
                if let Some(text) = text {
                    spotlight.typed(renderer, scene, self.dock.as_ref(), Some(text));
                }
            }
        }
    }

    /// Alt let go (`take`) or Escape: put the switcher away, and bring the
    /// picked window forward if taken.
    fn end_switch<const N: usize>(&mut self, scene: &mut Scene<N>, take: bool, now: u64) {
        let Some(switcher) = self.switcher.take() else {
            return;
        };
        let picked = switcher.picked();
        switcher.close(scene);
        if !take {
            println!("desktop    : switcher put away");
            return;
        }
        let index = picked.and_then(|id| self.windows.iter().position(|managed| managed.window == id));
        if let Some(index) = index {
            println!("desktop    : switched to window {}", self.windows[index].window);
            self.restore(scene, index, now);
        }
    }

    /// Do what a shortcut says.
    fn shortcut<const N: usize>(&mut self, scene: &mut Scene<N>, shortcut: Shortcut, now: u64) {
        println!("input      : shortcut {:?}", shortcut);
        let front = self
            .focus
            .filter(|index| self.windows.get(*index).is_some_and(|managed| !managed.docked));
        match shortcut {
            Shortcut::Close => {
                if let Some(index) = front {
                    crate::windows::push(self.windows[index].window, window::CLOSE, [0; 4]);
                }
            }
            Shortcut::Minimize => {
                if let Some(index) = front {
                    self.minimize(scene, index, now);
                }
            }
            Shortcut::Zoom => {
                if let Some(index) = front {
                    self.zoom(scene, index, now);
                }
            }
            Shortcut::Lock => launch(launcher::LOCK),
            Shortcut::Snap(snap) => {
                if let Some(index) = front.filter(|index| !self.windows[*index].popover) {
                    let rect = self.snap_rect(scene.screen(), snap);
                    let managed = &mut self.windows[index];
                    // Snapped again from snapped: back still means where it
                    // was before the first.
                    if managed.unzoomed.is_none() {
                        managed.unzoomed = Some((managed.at, (managed.width, managed.height)));
                    }
                    println!("desktop    : window {} snapped {:?}", managed.window, snap);
                    self.resize(scene, index, (rect.width, rect.height - BAR));
                    self.move_to(scene, index, (rect.x, rect.y), now);
                }
            }
            Shortcut::ShowDesktop => {
                let open: Vec<usize> = (0..self.windows.len())
                    .filter(|index| {
                        let managed = &self.windows[*index];
                        !managed.docked && !managed.popover && !managed.cover && !managed.minimized
                    })
                    .collect();
                if !open.is_empty() {
                    self.desktop_shown = open.iter().map(|index| self.windows[*index].window).collect();
                    for index in open {
                        self.minimize(scene, index, now);
                    }
                    println!("desktop    : showing the desktop, {} window(s) put away", self.desktop_shown.len());
                } else {
                    let back = core::mem::take(&mut self.desktop_shown);
                    // In the order they stood, so the one on top ends on top.
                    let mut count = 0;
                    for window in back {
                        if let Some(index) = self.windows.iter().position(|managed| managed.window == window) {
                            self.restore(scene, index, now);
                            count += 1;
                        }
                    }
                    println!("desktop    : {} window(s) back from the desktop", count);
                }
            }
            Shortcut::Unsnap => {
                if let Some(index) = front.filter(|index| self.windows[*index].unzoomed.is_some()) {
                    self.zoom(scene, index, now);
                }
            }
            Shortcut::Settings => {
                let open = self
                    .windows
                    .iter()
                    .rposition(|managed| !managed.docked && managed.name == "Settings");
                match (open, &self.dock) {
                    (Some(index), _) => self.restore(scene, index, now),
                    (None, Some(dock)) => {
                        if let Some(icon) = dock.find("Settings") {
                            dock.launch(icon, scene, now);
                        }
                    }
                    (None, None) => println!("desktop    : no dock to start Settings from"),
                }
            }
        }
    }

    /// Move window `index` to `at`, on a spring, every part of it.
    fn move_to<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, at: (i32, i32), now: u64) {
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        managed.at = at;
        let animate = self.theme.speed > 0;
        for (id, (x, y)) in managed.places(at, self.theme.buttons) {
            if let Some(layer) = scene.layer_mut(id) {
                let (x, y) = (Fx::from_int(x), Fx::from_int(y));
                if animate {
                    layer.x = layer.x.retarget(now, x, corrode_render::anim::Spring::SNAPPY);
                    layer.y = layer.y.retarget(now, y, corrode_render::anim::Spring::SNAPPY);
                } else {
                    layer.x = Animation::rest(x);
                    layer.y = Animation::rest(y);
                }
            }
        }
        // The glyphs were on the buttons where they were.
        self.hide_glyphs(scene);
    }

    /// Put window `index` away: it drops towards the dock and fades, and
    /// gives up the keyboard. Its dock icon brings it back.
    fn minimize<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, now: u64) {
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        if managed.docked || managed.minimized {
            return;
        }
        managed.minimized = true;
        println!("desktop    : window {} minimized", managed.window);
        let drop = scene.screen().height as i32;
        let ms = self.theme.ms(280);
        if ms > 0 {
            managed.fade_frame(scene, now + ms as u64 * 1_000_000);
        }
        let ids: Vec<(LayerId, (i32, i32))> = managed.places(managed.at, self.theme.buttons);
        for (id, (_, y)) in ids {
            if let Some(layer) = scene.layer_mut(id) {
                let from = layer.opacity.sample(now).value;
                if ms == 0 {
                    layer.opacity = Animation::rest(Fx::ZERO);
                    continue;
                }
                layer.opacity = Animation::ease(now, ms, from, Fx::ZERO, Ease::In);
                layer.y = Animation::ease(
                    now,
                    ms,
                    layer.y.sample(now).value,
                    Fx::from_int(y + drop / 3),
                    Ease::In,
                );
            }
        }
        if self.focus == Some(index) {
            self.set_focus(scene, None);
        }
        self.hide_glyphs(scene);
    }

    /// Bring window `index` back if it was put away, and give it the
    /// keyboard either way.
    fn restore<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, now: u64) {
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        if managed.minimized {
            managed.minimized = false;
            println!("desktop    : window {} restored", managed.window);
            let ms = self.theme.ms(250);
            if ms > 0 {
                managed.fade_frame(scene, now + ms as u64 * 1_000_000);
            }
            for (id, (_, y)) in managed.places(managed.at, self.theme.buttons) {
                if let Some(layer) = scene.layer_mut(id) {
                    if ms == 0 {
                        layer.opacity = Animation::rest(Fx::from_int(255));
                        layer.y = Animation::rest(Fx::from_int(y));
                        continue;
                    }
                    layer.opacity =
                        Animation::ease(now, ms, Fx::ZERO, Fx::from_int(255), Ease::Out);
                    layer.y = layer
                        .y
                        .retarget(now, Fx::from_int(y), corrode_render::anim::Spring::SMOOTH);
                }
            }
        }
        // Told again, so the focus paints over the fade's opacities.
        if self.focus == Some(index) {
            self.paint_focus(scene, index, true);
        } else {
            self.set_focus(scene, Some(index));
        }
    }

    /// The green button: centre the window in the room between the panel
    /// and the dock, or send it back to where it was. Windows cannot be
    /// resized yet -- their pixels are a fixed size -- so this is what
    /// zoom can mean for now.
    fn zoom<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, now: u64) {
        let screen = scene.screen();
        let dock = self.theme.dock;
        let can_resize = self.can_resize;
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        // The room between the panel and the dock.
        let (top, bottom) = (40, screen.height as i32 - if dock { 92 } else { 8 });
        let resizable = can_resize && crate::windows::resizable(managed.window);
        let (at, size) = match managed.unzoomed.take() {
            Some(back) => back,
            None => {
                managed.unzoomed = Some((managed.at, (managed.width, managed.height)));
                if resizable {
                    // Fill the room, a margin in from its edges.
                    let width = screen.width - 16;
                    let height = (bottom - top) as u32 - BAR;
                    ((8, top), (width, height))
                } else {
                    let total = (managed.height + managed.bar()) as i32;
                    (
                        (
                            (screen.width as i32 - managed.width as i32) / 2,
                            (top + (bottom - top - total) / 2).max(top),
                        ),
                        (managed.width, managed.height),
                    )
                }
            }
        };
        println!("desktop    : window {} zoomed", managed.window);
        let mut at = at;
        if resizable && size != (managed.width, managed.height) {
            self.resize(scene, index, size);
            // It may have come out smaller than asked: centred, then.
            let width = self.windows[index].width;
            if width < size.0 {
                at.0 += (size.0 - width) as i32 / 2;
            }
        }
        self.move_to(scene, index, at, now);
    }

    /// A snapped or zoomed window being dragged away: back to its size from
    /// before, held at the same fraction of its bar's width so it stays
    /// under the pointer. Where it goes back to is forgotten; the drag
    /// decides that now.
    fn restore_size<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, grab_x: i32, grab_y: i32) {
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        let Some((from, size)) = managed.unzoomed.take() else {
            return;
        };
        let wide = managed.width.max(1) as i32;
        if size != (managed.width, managed.height) {
            self.resize(scene, index, size);
        }
        let now_wide = self.windows[index].width as i32;
        let grab_x = grab_x * now_wide / wide;
        self.drag = Some((index, grab_x, grab_y));
        self.drag_from = from;
        println!(
            "desktop    : window {} unsnapped to {}x{}",
            self.windows[index].window, self.windows[index].width, self.windows[index].height
        );
    }

    /// Where a window snapped to `snap` goes, frame and all: the room
    /// between the menu bar and the dock, the half or the whole of it.
    fn snap_rect(&self, screen: Rect, snap: Snap) -> Rect {
        let top = 40;
        let bottom = screen.height as i32 - if self.theme.dock { 92 } else { 8 };
        let height = (bottom - top) as u32;
        let half = (screen.width - 24) / 2;
        match snap {
            Snap::Left => Rect::new(8, top, half, height),
            Snap::Right => Rect::new(16 + half as i32, top, half, height),
            Snap::Fill => Rect::new(8, top, screen.width - 16, height),
        }
    }

    /// The pointer moved while dragging window `index`: show, move or take
    /// away the preview of where it would snap.
    fn track_snap<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, now: u64) {
        let screen = scene.screen();
        let resizable = self.can_resize
            && self
                .windows
                .get(index)
                .is_some_and(|managed| crate::windows::resizable(managed.window));
        let zone = if !resizable {
            None
        } else if self.at.0 <= 2 {
            Some(Snap::Left)
        } else if self.at.0 >= screen.width as i32 - 3 {
            Some(Snap::Right)
        } else if self.at.1 <= 2 {
            Some(Snap::Fill)
        } else {
            None
        };
        match (zone, self.snap) {
            (None, None) => {}
            (None, Some((_, preview))) => {
                scene.remove(preview);
                self.snap = None;
            }
            (Some(zone), Some((shown, _))) if zone == shown => {}
            (Some(zone), Some((_, preview))) => {
                let rect = self.snap_rect(screen, zone);
                if let Some(layer) = scene.layer_mut(preview) {
                    for (animation, to) in [
                        (&mut layer.x, rect.x),
                        (&mut layer.y, rect.y),
                        (&mut layer.width, rect.width as i32),
                        (&mut layer.height, rect.height as i32),
                    ] {
                        *animation = animation.retarget(now, Fx::from_int(to), Spring::SNAPPY);
                    }
                }
                self.snap = Some((zone, preview));
            }
            (Some(zone), None) => {
                let rect = self.snap_rect(screen, zone);
                // It grows out of the pointer, as Plasma's does.
                let mut layer = pane(rect, self.theme.radius as u16, &self.theme);
                let ms = self.theme.ms(160);
                let strength = Fx::from_int(170);
                layer.opacity = if ms > 0 {
                    Animation::ease(now, ms, Fx::ZERO, strength, Ease::Out)
                } else {
                    Animation::rest(strength)
                };
                if ms > 0 {
                    let (x, y) = self.at;
                    for (animation, from, to) in [
                        (&mut layer.x, x, rect.x),
                        (&mut layer.y, y, rect.y),
                        (&mut layer.width, 0, rect.width as i32),
                        (&mut layer.height, 0, rect.height as i32),
                    ] {
                        *animation = Animation::spring(
                            now,
                            Fx::from_int(from),
                            Fx::from_int(to),
                            corrode_render::anim::Velocity::ZERO,
                            Spring::SNAPPY,
                        );
                    }
                }
                if let Some(preview) = scene.add(layer) {
                    // Behind the window being dragged.
                    if let Some(managed) = self.windows.get(index) {
                        let ids: Vec<LayerId> = managed.layers().collect();
                        for id in ids {
                            scene.raise(id);
                        }
                    }
                    self.snap = Some((zone, preview));
                }
            }
        }
    }

    /// The drag of window `index` is over: snap it where the preview was.
    fn end_snap<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, now: u64) {
        let Some((snap, preview)) = self.snap.take() else {
            return;
        };
        scene.remove(preview);
        let rect = self.snap_rect(scene.screen(), snap);
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        // Zoom puts it back where and as big as it was before the drag.
        managed.unzoomed = Some((self.drag_from, (managed.width, managed.height)));
        println!("desktop    : window {} snapped {:?}", managed.window, snap);
        self.resize(scene, index, (rect.width, rect.height - BAR));
        self.move_to(scene, index, (rect.x, rect.y), now);
    }

    /// Make window `index` `size`, its program and every layer of it.
    fn resize<const N: usize>(&mut self, scene: &mut Scene<N>, index: usize, size: (u32, u32)) {
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        let Some((width, height)) = crate::windows::resize(managed.window, size.0, size.1) else {
            return;
        };
        println!("desktop    : window {} is now {}x{}", managed.window, width, height);
        managed.width = width;
        managed.height = height;
        let blur = SHADOW_BLUR as u32;
        let sizes = [
            (managed.shadow, width + 2 * blur, height + BAR + 2 * blur),
            (
                managed.frame,
                width,
                managed.frame_rows(self.theme.radius as u32),
            ),
            (Some(managed.body), width, height),
        ];
        for (id, w, h) in sizes {
            if let Some(layer) = id.and_then(|id| scene.layer_mut(id)) {
                layer.width = Animation::rest(Fx::from_int(w as i32));
                layer.height = Animation::rest(Fx::from_int(h as i32));
                if let corrode_render::scene::Content::Surface {
                    width: sw,
                    height: sh,
                    stretch,
                    ..
                } = &mut layer.content
                {
                    *sw = w;
                    *sh = h;
                    // Cut to its new size from here on, not stretched.
                    *stretch = false;
                }
            }
        }
        self.resized.push(managed.surface);
    }

    /// Let zoom resize windows: only where the renderer can be shown a
    /// surface again at a new size.
    pub fn allow_resize(&mut self, allowed: bool) {
        self.can_resize = allowed;
    }

    /// Surfaces resized since the last call, to show the renderer again.
    pub fn take_resized(&mut self) -> Vec<usize> {
        core::mem::take(&mut self.resized)
    }

    /// The theme changed: restyle every window and the dock to match it, and
    /// tell every program.
    pub fn restyle<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: Theme,
    ) {
        use corrode_render::scene::Content;

        self.theme = theme;
        self.hide_glyphs(scene);
        for index in 0..self.windows.len() {
            let managed = &mut self.windows[index];
            crate::windows::push(managed.window, window::THEME, theme.to_words());
            let radius = if managed.docked { 0 } else { theme.radius as u16 };
            let width = managed.width;
            let height = managed.frame_rows(radius as u32);
            if let Some(id) = managed.frame {
                let fresh = pane(Rect::new(0, 0, width, height), radius, &theme);
                if let Some(layer) = scene.layer_mut(id) {
                    layer.height = Animation::rest(Fx::from_int(height as i32));
                }
                // The old frame may have been taller: all of it is stale.
                scene.damage(id, Rect::new(0, 0, width, managed.height + managed.bar()));
                if let Some(layer) = scene.layer_mut(id) {
                    layer.content = fresh.content;
                    layer.color = fresh.color;
                }
                scene.damage(id, Rect::new(0, 0, width, height));
            }
            if let Some(layer) = scene.layer_mut(managed.body) {
                if let Content::Surface { radius: corner, .. } = &mut layer.content {
                    *corner = radius.min(255) as u8;
                }
            }
            if let Some(layer) = managed.shadow.and_then(|id| scene.layer_mut(id)) {
                if let Content::Shadow { radius: corner, .. } = &mut layer.content {
                    *corner = radius;
                }
            }
            for (light, button) in managed.lights.iter_mut().zip(Button::order(theme.buttons)) {
                if let Some((which, _)) = light {
                    *which = button;
                }
            }
            if let Some(layer) = managed.title.and_then(|(id, ..)| scene.layer_mut(id)) {
                layer.color = theme.text();
            }
            if !managed.minimized {
                for (id, (x, y)) in managed.places(managed.at, theme.buttons) {
                    if let Some(layer) = scene.layer_mut(id) {
                        layer.x = Animation::rest(Fx::from_int(x));
                        layer.y = Animation::rest(Fx::from_int(y));
                    }
                }
            }
            self.paint_focus(scene, index, self.focus == Some(index));
        }

        // The dock is drawn from the theme -- its size, whether it is there
        // at all -- so it is drawn again.
        if let Some(dock) = self.dock.take() {
            let ids: Vec<LayerId> = dock.layers().collect();
            for id in ids {
                scene.remove(id);
            }
        }
        self.dock = crate::dock::Dock::new(renderer, scene, &theme);
        self.show_running(scene);
        self.restack(scene);
    }

    /// The surfaces of the windows that have gone since the last call.
    pub fn take_closed(&mut self) -> Vec<usize> {
        core::mem::take(&mut self.closed)
    }

    /// A right click on a dock icon: its menu, above it. False if the
    /// pointer is not on one.
    fn dock_menu<const N: usize>(&mut self, scene: &Scene<N>) -> bool {
        let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
        let top = scene
            .layers_at(self.at.0, self.at.1)
            .find(|id| !pointer.contains(id));
        let Some((dock, icon)) = self
            .dock
            .as_ref()
            .and_then(|dock| top.and_then(|top| dock.icon(top)).map(|icon| (dock, icon)))
        else {
            return false;
        };
        let Some((x, y)) = dock.at(icon) else {
            return false;
        };
        let mut title = [0u8; 8];
        for (slot, byte) in title.iter_mut().zip(dock.title(icon).bytes()) {
            *slot = byte;
        }
        crate::context::set([
            crate::context::DOCK,
            dock.program(icon),
            dock.is_running(icon) as u64,
            u64::from_le_bytes(title),
        ]);
        self.menu_at = (x + dock.size() as i32 / 2, y - 10);
        self.menu_above = true;
        println!("input      : right click on {} in the dock", dock.title(icon));
        launch(launcher::MENU);
        true
    }

    /// Whether the pointer is on the bare desktop: no window under it (the
    /// wallpaper is a layer too, so "nothing" is not the test), nor the
    /// dock, nor a notification.
    fn on_desktop<const N: usize>(&self, scene: &Scene<N>) -> bool {
        if self.under(scene).is_ok() {
            return false;
        }
        let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
        let Some(top) = scene
            .layers_at(self.at.0, self.at.1)
            .find(|id| !pointer.contains(id))
        else {
            return true;
        };
        let on_dock = self.dock.as_ref().is_some_and(|dock| dock.layers().any(|id| id == top));
        let on_banner = self.banner.as_ref().is_some_and(|banner| banner.contains(top));
        !on_dock && !on_banner
    }

    /// Tell a popover under the pointer where on it the pointer is, so a
    /// menu can light the item under it. Only popovers hear of it: they
    /// are small, and there is at most one.
    fn hover_popover<const N: usize>(&mut self, scene: &Scene<N>) {
        let over = match self.under(scene) {
            Ok((index, Part::Body)) if self.windows[index].popover => {
                let managed = &self.windows[index];
                Some((managed.window, self.at.0 - managed.at.0, self.at.1 - managed.at.1))
            }
            _ => None,
        };
        if over == self.hovered_at {
            return;
        }
        // Off it: told once, with a place on no item.
        if let (None, Some((window, ..))) = (over, self.hovered_at) {
            crate::windows::push(window, window::HOVER, [-1i64 as u64, -1i64 as u64, 0, 0]);
        }
        if let Some((window, x, y)) = over {
            crate::windows::push(window, window::HOVER, [x as i64 as u64, y as i64 as u64, 0, 0]);
        }
        self.hovered_at = over;
    }

    /// Tell the dock which icon, if any, the pointer is over.
    fn dock_hover<const N: usize>(&mut self, scene: &mut Scene<N>, now: u64) {
        // The dock is out of sight under a cover; whatever the pointer is
        // "over" there is the cover. (The top-layer check below sees that
        // too, but a cover that is still fading in may not be on top yet.)
        // Nor while a context menu is up: its label would sit on the menu
        // of the very icon it names.
        let menu_open = self
            .windows
            .iter()
            .any(|managed| managed.popover && crate::windows::menu(managed.window));
        if self.covered().is_some() || menu_open {
            if let Some(dock) = &mut self.dock {
                dock.hover(scene, None, now);
            }
            return;
        }
        let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
        let top = scene
            .layers_at(self.at.0, self.at.1)
            .find(|id| !pointer.contains(id));
        if let Some(dock) = &mut self.dock {
            let over = top.and_then(|id| dock.icon(id));
            dock.hover(scene, over, now);
        }
    }

    /// Take the glyphs off the buttons: the next hover puts them back
    /// wherever the buttons are now.
    fn hide_glyphs<const N: usize>(&mut self, scene: &mut Scene<N>) {
        self.hovering = None;
        for glyph in self.glyphs.iter().flatten() {
            if let Some(layer) = scene.layer_mut(*glyph) {
                layer.opacity = Animation::rest(Fx::ZERO);
            }
        }
    }

    /// Light the dock's dots for the programs with a window open.
    fn show_running<const N: usize>(&mut self, scene: &mut Scene<N>) {
        if let Some(dock) = &mut self.dock {
            dock.running(scene, self.windows.iter().map(|managed| managed.name.as_str()));
        }
    }

    /// Put the pointer's layers where it is, as the cursor it should be:
    /// a resize arrow over a window's edge, or while dragging one.
    fn place_pointer<const N: usize>(&mut self, scene: &mut Scene<N>) {
        let cursor = match (&self.sizing, self.drag) {
            (Some(sizing), _) => Cursor::of((sizing.left, sizing.top, sizing.right, sizing.bottom)),
            (None, Some(_)) => Cursor::Arrow,
            (None, None) if self.resize_cursors => match self.under(scene) {
                Ok((index, part)) => Cursor::of(self.edges(index, part)),
                Err(_) => Cursor::Arrow,
            },
            _ => Cursor::Arrow,
        };
        if cursor != self.cursor {
            println!("input      : the pointer is now {:?}", cursor);
            self.cursor = cursor;
        }
        let (stencils, hot) = cursor.stencils();
        let (x, y) = (self.at.0 - hot.0, self.at.1 - hot.1);
        let Some(layers) = self.pointer else { return };
        for (id, src) in layers.into_iter().zip(stencils) {
            if let Some(layer) = scene.layer_mut(id) {
                layer.x = Animation::rest(Fx::from_int(x));
                layer.y = Animation::rest(Fx::from_int(y));
                layer.width = Animation::rest(Fx::from_int(src.width as i32));
                layer.height = Animation::rest(Fx::from_int(src.height as i32));
                layer.content = corrode_render::scene::Content::Mask { src };
            }
        }
    }

    /// Which edges of window `index` a press on `part` at the pointer would
    /// pick up -- left, top, right, bottom -- none if it cannot be resized.
    /// The top edge is the title bar's own top, which would otherwise
    /// start a drag.
    fn edges(&self, index: usize, part: Part) -> (bool, bool, bool, bool) {
        let none = (false, false, false, false);
        let Some(managed) = self.windows.get(index) else {
            return none;
        };
        if !self.can_resize
            || !matches!(part, Part::Body | Part::Bar)
            || !crate::windows::resizable(managed.window)
        {
            return none;
        }
        let (left, top) = managed.at;
        let (right, bottom) = (
            left + managed.width as i32,
            top + (managed.bar() + managed.height) as i32,
        );
        let near_top = part == Part::Bar && self.at.1 < top + TOP_EDGE;
        // Along the bar, only its top edge and corners are edges.
        let sides = part == Part::Body || near_top;
        (
            sides && self.at.0 < left + EDGE,
            near_top,
            sides && self.at.0 >= right - EDGE,
            part == Part::Body && self.at.1 >= bottom - EDGE,
        )
    }

    fn show_pointer<const N: usize>(&mut self, scene: &mut Scene<N>, now: u64) {
        let Some(layers) = self.pointer else { return };
        for id in layers {
            if let Some(layer) = scene.layer_mut(id) {
                if layer.opacity.sample(now).value == Fx::ZERO {
                    layer.opacity =
                        Animation::ease(now, 200, Fx::ZERO, Fx::from_int(255), Ease::Out);
                }
            }
        }
    }

    fn pointer<const N: usize>(
        &mut self,
        scene: &mut Scene<N>,
        now: u64,
        dx: i32,
        dy: i32,
        buttons: u8,
    ) {
        let screen = scene.screen();
        self.at = (
            (self.at.0 + dx).clamp(0, screen.width as i32 - 1),
            (self.at.1 + dy).clamp(0, screen.height as i32 - 1),
        );
        self.place_pointer(scene);
        self.show_pointer(scene, now);
        if !self.moved && (dx != 0 || dy != 0) {
            self.moved = true;
            println!(
                "input      : the pointer moved to ({}, {})",
                self.at.0, self.at.1
            );
        }
        // Dragging an edge: the size follows the pointer, within the screen.
        if let Some(sizing) = self.sizing {
            if buttons & 1 == 0 {
                self.sizing = None;
            } else if let Some(managed) = self.windows.get(sizing.index) {
                let grow = |delta: i32, size: u32, least: u32, most: i32| -> u32 {
                    (size as i32 + delta).clamp(least as i32, most.max(least as i32)) as u32
                };
                let (dx, dy) = (self.at.0 - sizing.from.0, self.at.1 - sizing.from.1);
                let right = managed.at.0 + managed.width as i32;
                let bottom = managed.at.1 + (managed.bar() + managed.height) as i32;
                // A left or top edge grows the window as the pointer goes
                // away from it, up to the screen's edge.
                let (width, anchor_x) = if sizing.left {
                    (grow(-dx, sizing.size.0, MIN_SIZE.0, right), Some(right))
                } else if sizing.right {
                    (grow(dx, sizing.size.0, MIN_SIZE.0, screen.width as i32 - managed.at.0), None)
                } else {
                    (managed.width, None)
                };
                let (height, anchor_y) = if sizing.top {
                    let most = bottom - managed.bar() as i32;
                    (grow(-dy, sizing.size.1, MIN_SIZE.1, most), Some(bottom))
                } else if sizing.bottom {
                    let most = screen.height as i32 - managed.at.1 - managed.bar() as i32;
                    (grow(dy, sizing.size.1, MIN_SIZE.1, most), None)
                } else {
                    (managed.height, None)
                };
                self.wanted = Some((sizing.index, (width, height), (anchor_x, anchor_y)));
            }
        }
        // Dragging a window by its bar: it follows the pointer, held where
        // it was picked up.
        if let (Some((index, grab_x, grab_y)), true) = (self.drag, self.unsnap && buttons & 1 != 0) {
            self.unsnap = false;
            self.restore_size(scene, index, grab_x, grab_y);
        }
        if let Some((index, grab_x, grab_y)) = self.drag {
            if buttons & 1 == 0 {
                self.drag = None;
                self.end_snap(scene, index, now);
            } else if let Some(managed) = self.windows.get_mut(index) {
                managed.at = (self.at.0 - grab_x, (self.at.1 - grab_y).max(0));
                let managed = &self.windows[index];
                for (id, (x, y)) in managed.places(managed.at, self.theme.buttons) {
                    if let Some(layer) = scene.layer_mut(id) {
                        layer.x = Animation::rest(Fx::from_int(x));
                        layer.y = Animation::rest(Fx::from_int(y));
                    }
                }
                // The glyphs go with the buttons they are on.
                self.hide_glyphs(scene);
                self.track_snap(scene, index, now);
            }
        }
        self.hover(scene);
        self.dock_hover(scene, now);
        self.hover_popover(scene);

        let pressed = buttons & !self.buttons;
        let released = self.buttons & !buttons;
        if pressed != 0 || released != 0 {
            self.click(scene, buttons, pressed, now);
        }
        self.buttons = buttons;
    }
}
