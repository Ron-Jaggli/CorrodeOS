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
//! title bar; clicking a window gives it the keyboard and brings it to the
//! front, and dragging its bar moves it. Keys go to the window with the
//! keyboard, or, when none has it, to a line of typed text on the boot
//! screen.

use alloc::string::String;
use alloc::vec::Vec;

use corrode_abi::{ps2, window};
use corrode_render::anim::{Animation, Ease, Fx};
use corrode_render::glass::Glass;
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_text::{Coverage, Style};

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
/// The close button's cross, to the right of the pointer's two stencils.
const CLOSE_SRC: Rect = Rect::new(
    2 * POINTER_SIZE.0 as i32,
    ATLAS_ROW as i32,
    CLOSE_SIZE,
    CLOSE_SIZE,
);
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
const BAR: u32 = 22;
const BAR_FOCUSED: Color = Color::rgb(0xb7, 0x41, 0x0e);
const BAR_IDLE: Color = Color::rgb(0x3a, 0x3d, 0x45);
/// The close button: a cross at the right of the bar, kept in the atlas
/// beside the pointer.
const CLOSE_SIZE: u32 = 12;
const CLOSE_COLOR: Color = Color::rgb(0xf2, 0xee, 0xe8);

/// The glass the desktop's own panes are made of.
const PANE: Glass = Glass {
    radius: 0,
    ..Glass::regular(true)
};
/// The dock: a pane floating above the bottom edge.
const DOCK_WIDTH: u32 = 360;
const DOCK_HEIGHT: u32 = 64;
const DOCK_GAP: i32 = 10;

/// The close button's cross, as coverage out of 255: both diagonals, a
/// pixel and three quarters thick, sampled four by four per pixel.
fn cross() -> Vec<u8> {
    let size = CLOSE_SIZE as i32;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In eighths of a pixel, so every sample is a whole number.
    let (low, high) = (2 * 8, (size - 2) * 8);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px * 8 + sx * 2 + 1, py * 8 + sy * 2 + 1);
                    let inside = (low..=high).contains(&x) && (low..=high).contains(&y);
                    // Ten eighths along an axis is about seven across a
                    // diagonal: half the stroke.
                    let on_line = (x - y).abs() <= 10 || (x + y - size * 8).abs() <= 10;
                    hits += (inside && on_line) as i32;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// A program's window, as the painter keeps it.
struct Managed {
    window: usize,
    surface: usize,
    /// The title bar, which a docked window does not have.
    bar: Option<LayerId>,
    /// The close button on the bar.
    close: Option<LayerId>,
    /// A pane of glass behind the window, for a docked one.
    pane: Option<LayerId>,
    body: LayerId,
    /// Where the window's top left is -- its bar's, if it has one.
    at: (i32, i32),
    width: u32,
    docked: bool,
}

impl Managed {
    fn body_at(&self) -> (i32, i32) {
        let bar = if self.bar.is_some() { BAR as i32 } else { 0 };
        (self.at.0, self.at.1 + bar)
    }

    /// Where the close button goes, for a window whose bar is at `at`.
    fn close_at(at: (i32, i32), width: u32) -> (i32, i32) {
        let inset = ((BAR - CLOSE_SIZE) / 2) as i32;
        (at.0 + width as i32 - CLOSE_SIZE as i32 - inset, at.1 + inset)
    }

    /// Every layer the window is drawn with, bottom first.
    fn layers(&self) -> impl Iterator<Item = LayerId> {
        [self.pane, self.bar, self.close, Some(self.body)]
            .into_iter()
            .flatten()
    }
}

/// The keyboard and mouse, as the scene shows them, and the windows they
/// act on.
pub struct Desk {
    /// Outline below, fill above, moving together.
    pointer: Option<[LayerId; 2]>,
    /// Whether the close button's cross made it into the atlas.
    cross: bool,
    /// The dock's pane, along the bottom of the screen.
    dock: Option<LayerId>,
    at: (i32, i32),
    buttons: u8,
    line: Option<LayerId>,
    typed: String,
    line_height: u32,
    /// Whether the first movement has been reported.
    moved: bool,
    windows: Vec<Managed>,
    /// Which of `windows` has the keyboard.
    focus: Option<usize>,
    /// A window being dragged by its bar, and where on the bar it was held.
    drag: Option<(usize, i32, i32)>,
    /// Windows placed so far, for staggering the next one.
    placed: i32,
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

        let cross = renderer
            .upload_mask(CLOSE_SRC, &cross())
            .is_ok();

        let dock = scene.add(Layer::glass(
            Rect::new(
                (screen.width as i32 - DOCK_WIDTH as i32) / 2,
                screen.height as i32 - DOCK_HEIGHT as i32 - DOCK_GAP,
                DOCK_WIDTH,
                DOCK_HEIGHT,
            ),
            Glass {
                radius: 22,
                ..PANE
            },
        ));

        let line_height = corrode_text::measure("> ", LINE_STYLE).height;
        let mut desk = Self {
            pointer,
            cross,
            dock,
            at: (screen.width as i32 / 2, screen.height as i32 / 2),
            buttons: 0,
            line: None,
            typed: String::new(),
            line_height,
            moved: false,
            windows: Vec::new(),
            focus: None,
            drag: None,
            placed: 0,
        };
        let src = Rect::new(0, LINE_ROW as i32, 1, line_height);
        let y = screen.height as i32 - 40 - line_height as i32;
        desk.line = scene.add(Layer::mask(48, y, src, LINE_COLOR));
        desk.redraw_line(renderer, scene);
        desk
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

    /// Give the painter a window the server has made: a place, a bar, and
    /// the keyboard. It rises into place and fades in, like everything
    /// else that arrives.
    pub fn manage<const N: usize>(&mut self, window: usize, scene: &mut Scene<N>, now: u64) {
        let Some((surface, width, height, docked)) = crate::windows::shape(window) else {
            return;
        };
        let at = if docked {
            (0, 0)
        } else {
            let step = self.placed % 6;
            self.placed += 1;
            (120 + 40 * step, 90 + 36 * step)
        };
        let bar = (!docked)
            .then(|| scene.add(Layer::solid(Rect::new(at.0, at.1, width, BAR), BAR_IDLE)))
            .flatten();
        let close = (bar.is_some() && self.cross)
            .then(|| {
                let (x, y) = Managed::close_at(at, width);
                scene.add(Layer::mask(x, y, CLOSE_SRC, CLOSE_COLOR))
            })
            .flatten();
        let mut managed_body_y = at.1;
        if bar.is_some() {
            managed_body_y += BAR as i32;
        }
        let translucent = crate::windows::translucent(window);
        // A docked window is a pane of glass with the program's pixels
        // over it; transparent ones let the glass through.
        let pane = docked
            .then(|| scene.add(Layer::glass(Rect::new(at.0, at.1, width, height), PANE)))
            .flatten();
        let make = if translucent {
            Layer::translucent_surface
        } else {
            Layer::surface
        };
        let Some(body) = scene.add(make(at.0, managed_body_y, surface as u32, width, height)) else {
            println!("desktop    : no room in the scene for another window");
            for id in [bar, close, pane].into_iter().flatten() {
                scene.remove(id);
            }
            return;
        };
        if !docked {
            for id in [bar, close, Some(body)].into_iter().flatten() {
                if let Some(layer) = scene.layer_mut(id) {
                    let rest = layer.y.sample(now).value;
                    layer.opacity =
                        Animation::ease(now, 250, Fx::ZERO, Fx::from_int(255), Ease::Out);
                    layer.y = Animation::spring(
                        now,
                        Fx::from_int(rest.round() + 24),
                        rest,
                        corrode_render::anim::Velocity::ZERO,
                        corrode_render::anim::Spring::SMOOTH,
                    );
                }
            }
        }
        self.windows.push(Managed {
            window,
            surface,
            bar,
            close,
            pane,
            body,
            at,
            width,
            docked,
        });
        if !docked {
            self.set_focus(scene, Some(self.windows.len() - 1));
        }
    }

    /// Give the keyboard to window `index`, or to nobody. The window that
    /// gets it comes to the front; both are told.
    fn set_focus<const N: usize>(&mut self, scene: &mut Scene<N>, index: Option<usize>) {
        if self.focus == index {
            return;
        }
        for (which, gained) in [(self.focus, false), (index, true)] {
            let Some(managed) = which.and_then(|which| self.windows.get(which)) else {
                continue;
            };
            crate::windows::push(managed.window, window::FOCUS, [gained as u64, 0, 0, 0]);
            if let Some(layer) = managed.bar.and_then(|bar| scene.layer_mut(bar)) {
                layer.color = if gained { BAR_FOCUSED } else { BAR_IDLE };
            }
            if gained {
                for id in managed.layers() {
                    scene.raise(id);
                }
            }
        }
        self.focus = index;
        self.restack(scene);
    }

    /// Docked windows above every other window, and the pointer above all.
    fn restack<const N: usize>(&self, scene: &mut Scene<N>) {
        let wanted = self.pointer.map(|layers| layers[1]);
        if wanted.is_some() && scene.top() == wanted {
            return;
        }
        if let Some(dock) = self.dock {
            scene.raise(dock);
        }
        for managed in self.windows.iter().filter(|managed| managed.docked) {
            for id in managed.layers() {
                scene.raise(id);
            }
        }
        if let Some([below, above]) = self.pointer {
            scene.raise(below);
            scene.raise(above);
        }
    }

    /// A button went down or up. What is under the pointer decides what it
    /// means: a bar is picked up, a window body is given the keyboard and
    /// told, and anything else takes the keyboard away from every window.
    fn click<const N: usize>(&mut self, scene: &mut Scene<N>, buttons: u8, pressed: u8) {
        // The pointer is always on top and always under itself, so it is
        // looked past.
        let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
        let under = scene
            .layers_at(self.at.0, self.at.1)
            .find(|id| !pointer.contains(id));
        let hit = under.and_then(|id| {
            self.windows
                .iter()
                .position(|managed| managed.layers().any(|layer| layer == id))
                .map(|index| (index, id != self.windows[index].body))
        });
        let on_close = hit.is_some_and(|(index, _)| under == self.windows[index].close);

        if pressed & 1 != 0 {
            println!(
                "input      : click at ({}, {}) on {}",
                self.at.0,
                self.at.1,
                match hit {
                    Some(_) if on_close => "a close button",
                    Some((_, true)) => "a title bar",
                    Some(_) => "a window",
                    None if under.is_some() => "a layer",
                    None => "the background",
                }
            );
            match hit {
                Some((index, on_bar)) => {
                    if !self.windows[index].docked {
                        self.set_focus(scene, Some(index));
                    }
                    if on_close {
                        // Asked, not done: the window goes when its
                        // program does.
                        crate::windows::push(self.windows[index].window, window::CLOSE, [0; 4]);
                    } else if on_bar {
                        let (x, y) = self.windows[index].at;
                        self.drag = Some((index, self.at.0 - x, self.at.1 - y));
                    }
                }
                None => self.set_focus(scene, None),
            }
        }

        // A click on a window's body is the program's to interpret.
        if let Some((index, false)) = hit {
            let managed = &self.windows[index];
            let (x, y) = managed.body_at();
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
            println!("desktop    : window {} closed", managed.window);
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
                let (x, y) = managed.at;
                let (body_x, body_y) = managed.body_at();
                let width = managed.width;
                for (id, (x, y)) in [
                    (managed.bar, (x, y)),
                    (managed.close, Managed::close_at((x, y), width)),
                    (Some(managed.body), (body_x, body_y)),
                ] {
                    if let Some(layer) = id.and_then(|id| scene.layer_mut(id)) {
                        layer.x = Animation::rest(Fx::from_int(x));
                        layer.y = Animation::rest(Fx::from_int(y));
                    }
                }
            }
        }

        let pressed = buttons & !self.buttons;
        let released = self.buttons & !buttons;
        if pressed != 0 || released != 0 {
            self.click(scene, buttons, pressed);
        }
        self.buttons = buttons;
    }
}
