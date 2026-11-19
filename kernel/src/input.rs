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

use corrode_abi::{ps2, window};
use corrode_render::anim::{Animation, Ease, Fx};
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
    Pointer { dx: i32, dy: i32, buttons: u8 },
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
            Event::Pointer { dx, dy, buttons },
            Some(Some(Event::Pointer {
                dx: last_x,
                dy: last_y,
                buttons: last_buttons,
            })),
        ) = (event, self.last_mut().map(|last| *last))
        {
            if buttons == last_buttons {
                if let Some(last) = self.last_mut() {
                    *last = Some(Event::Pointer {
                        dx: last_x.saturating_add(dx),
                        dy: last_y.saturating_add(dy),
                        buttons,
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
const TITLE_SLOTS: usize = 2 * ((512 - TITLE_ROW) / TITLE_SLOT.1) as usize;
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

/// What the desktop does with Alt and a key, whatever window is in front.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shortcut {
    /// Alt+Tab: the next window to the front, back from the dock if need be.
    Next,
    /// Alt+Q, Alt+W, Alt+F4: ask the window in front to close.
    Close,
    /// Alt+M.
    Minimize,
    /// Alt+Z.
    Zoom,
    /// Alt+comma, as Cmd+comma opens an app's settings on macOS.
    Settings,
}

impl Shortcut {
    /// By scancode, set 1.
    fn of(key: u8) -> Option<Shortcut> {
        Some(match key {
            0x0f => Shortcut::Next,
            0x10 | 0x11 | 0x3e => Shortcut::Close,
            0x32 => Shortcut::Minimize,
            0x2c => Shortcut::Zoom,
            0x33 => Shortcut::Settings,
            _ => return None,
        })
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
    /// Where it was before its green button centred it, to go back to.
    unzoomed: Option<(i32, i32)>,
    body: LayerId,
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
    fn bar(&self) -> u32 {
        if self.docked {
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
    theme: Theme,
    windows: Vec<Managed>,
    /// Which of `windows` has the keyboard.
    focus: Option<usize>,
    /// A window being dragged by its bar, and where on the bar it was held.
    drag: Option<(usize, i32, i32)>,
    /// Windows placed so far, for staggering the next one.
    placed: i32,
    /// The surfaces of windows that have gone, for the painter to retire.
    closed: Vec<usize>,
    /// What docked windows were last told has the keyboard.
    announced: Option<String>,
}

impl Desk {
    /// Put the pointer's stencils in the atlas and its layers in the scene,
    /// hidden until a mouse turns up, and an empty line to type on.
    pub fn new<R: Renderer, const N: usize>(renderer: &mut R, scene: &mut Scene<N>) -> Self {
        let screen = scene.screen();
        let (width, height) = POINTER_SIZE;
        let mut pointer = None;
        let outline = Rect::new(0, ATLAS_ROW as i32, width, height);
        let fill = Rect::new(width as i32, ATLAS_ROW as i32, width, height);
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
            placed: 0,
            closed: Vec::new(),
            announced: None,
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
        self.forget_abandoned(scene);

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
                Event::Pointer { dx, dy, buttons } => {
                    self.pointer(scene, now, dx, dy, buttons);
                    self.restack(scene);
                }
            }
        }
        if typed {
            self.redraw_line(renderer, scene);
        }
        self.announce();
    }

    /// Tell docked windows -- the menu bar -- which app is in front, when
    /// that has changed since they were last told.
    fn announce(&mut self) {
        let front = self
            .focus
            .and_then(|index| self.windows.get(index))
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
        if title.is_empty() {
            return None;
        }
        let slot = (0..TITLE_SLOTS).find(|slot| self.titles & 1 << slot == 0)?;
        let (width, height) = TITLE_SLOT;
        let extent = corrode_text::measure(title, TITLE_STYLE);
        if extent.width > width || extent.height > height {
            return None;
        }
        let mut coverage = alloc::vec![0u8; (width * height) as usize];
        corrode_text::draw(title, TITLE_STYLE, &mut Coverage::new(&mut coverage, width as usize));
        renderer.upload_mask(Self::title_src(slot, width), &coverage).ok()?;
        self.titles |= 1 << slot;
        Some((slot, extent.width))
    }

    fn title_src(slot: usize, width: u32) -> Rect {
        let (slot_width, slot_height) = TITLE_SLOT;
        Rect::new(
            (slot % 2) as i32 * slot_width as i32,
            TITLE_ROW as i32 + (slot / 2) as i32 * slot_height as i32,
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
        let at = if docked {
            (0, 0)
        } else {
            let step = self.placed % 6;
            self.placed += 1;
            (120 + 40 * step, 90 + 36 * step)
        };
        let radius = if docked { 0 } else { theme.radius as u16 };
        let translucent = crate::windows::translucent(window);

        // Bottom first, so each lands above the last. The shadow is there
        // even with shadows off, unseen, so turning them on can show it in
        // its place under the window.
        let shadow = (!docked)
            .then(|| {
                scene.add(Layer::shadow(0, 0, width, height + BAR, radius, SHADOW_BLUR))
            })
            .flatten();
        let frame_height = height + if docked { 0 } else { BAR };
        let frame = scene.add(pane(Rect::new(0, 0, width, frame_height), radius, &theme));
        let mut lights = [None; 3];
        if !docked && self.stencils {
            for (slot, button) in lights.iter_mut().zip(Button::order(theme.buttons)) {
                *slot = scene
                    .add(Layer::mask(0, 0, DOT_SRC, button.color()))
                    .map(|id| (button, id));
            }
        }
        let name = crate::windows::title(window);
        let title = if docked {
            None
        } else {
            self.draw_title(renderer, &name)
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
        };
        let rise = theme.ms(250);
        for (id, (x, y)) in managed.places(at, theme.buttons) {
            if let Some(layer) = scene.layer_mut(id) {
                layer.x = Animation::rest(Fx::from_int(x));
                layer.y = Animation::rest(Fx::from_int(y));
                if !docked && rise > 0 {
                    layer.opacity =
                        Animation::ease(now, rise, Fx::ZERO, Fx::from_int(255), Ease::Out);
                    layer.y = Animation::spring(
                        now,
                        Fx::from_int(y + 24),
                        Fx::from_int(y),
                        corrode_render::anim::Velocity::ZERO,
                        corrode_render::anim::Spring::SMOOTH,
                    );
                }
            }
        }
        self.windows.push(managed);
        if docked {
            self.announced = None;
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
        // The dock first: it is above every window.
        if pressed & 1 != 0 {
            let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
            let top = scene
                .layers_at(self.at.0, self.at.1)
                .find(|id| !pointer.contains(id));
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
                        Part::Bar => {
                            let (x, y) = self.windows[index].at;
                            self.drag = Some((index, self.at.0 - x, self.at.1 - y));
                        }
                        Part::Body | Part::Shadow => {}
                    }
                }
                Err(_) => self.set_focus(scene, None),
            }
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
    fn forget_abandoned<const N: usize>(&mut self, scene: &mut Scene<N>) {
        let mut index = 0;
        while index < self.windows.len() {
            if !crate::windows::abandoned(self.windows[index].window) {
                index += 1;
                continue;
            }
            let managed = self.windows.remove(index);
            for id in managed.layers() {
                scene.remove(id);
            }
            if let Some((_, slot, _)) = managed.title {
                self.titles &= !(1 << slot);
            }
            println!("desktop    : window {} closed", managed.window);
            self.closed.push(managed.surface);
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

    /// Do what a shortcut says.
    fn shortcut<const N: usize>(&mut self, scene: &mut Scene<N>, shortcut: Shortcut, now: u64) {
        println!("input      : shortcut {:?}", shortcut);
        let front = self
            .focus
            .filter(|index| self.windows.get(*index).is_some_and(|managed| !managed.docked));
        match shortcut {
            Shortcut::Next => {
                let count = self.windows.len();
                let start = front.map_or(0, |index| index + 1);
                let next = (0..count)
                    .map(|step| (start + step) % count)
                    .find(|index| !self.windows[*index].docked && Some(*index) != front);
                if let Some(index) = next {
                    self.restore(scene, index, now);
                }
            }
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
        let Some(managed) = self.windows.get_mut(index) else {
            return;
        };
        let at = match managed.unzoomed.take() {
            Some(back) => back,
            None => {
                managed.unzoomed = Some(managed.at);
                let total = (managed.height + managed.bar()) as i32;
                let (top, bottom) = (40, screen.height as i32 - 90);
                (
                    (screen.width as i32 - managed.width as i32) / 2,
                    (top + (bottom - top - total) / 2).max(top),
                )
            }
        };
        println!("desktop    : window {} zoomed", managed.window);
        self.move_to(scene, index, at, now);
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
            let (width, height) = (managed.width, managed.height + managed.bar());
            if let Some(id) = managed.frame {
                let fresh = pane(Rect::new(0, 0, width, height), radius, &theme);
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
        // The tip is a pixel in from the stencil's corner.
        let (x, y) = (self.at.0 - 1, self.at.1 - 1);
        if let Some(layers) = self.pointer {
            for id in layers {
                if let Some(layer) = scene.layer_mut(id) {
                    layer.x = Animation::rest(Fx::from_int(x));
                    layer.y = Animation::rest(Fx::from_int(y));
                }
            }
        }
        self.show_pointer(scene, now);
        if !self.moved && (dx != 0 || dy != 0) {
            self.moved = true;
            println!(
                "input      : the pointer moved to ({}, {})",
                self.at.0, self.at.1
            );
        }
        // Dragging a window by its bar: it follows the pointer, held where
        // it was picked up.
        if let Some((index, grab_x, grab_y)) = self.drag {
            if buttons & 1 == 0 {
                self.drag = None;
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
            }
        }
        self.hover(scene);

        let pressed = buttons & !self.buttons;
        let released = self.buttons & !buttons;
        if pressed != 0 || released != 0 {
            self.click(scene, buttons, pressed, now);
        }
        self.buttons = buttons;
    }
}
