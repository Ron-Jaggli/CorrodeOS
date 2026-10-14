//! Keyboard and mouse, from the PS/2 driver in ring 3 to the screen.
//!
//! The driver sends one message per key or mouse movement to an endpoint. A
//! kernel task holds the receive half and puts what arrives in a queue; the
//! painter empties the queue when it wakes and changes the scene to match.
//! The task exists because a send is a rendezvous and the painter does not
//! block: it sleeps until the next tick, then paints whatever has happened.
//!
//! What the scene shows for now is a pointer that follows the mouse, kept on
//! top of everything, and one line of typed text. Where a key should go --
//! which window has focus -- is the window manager's question, and there is
//! no window manager yet.

use alloc::string::String;
use alloc::vec::Vec;

use corrode_abi::ps2;
use corrode_render::anim::{Animation, Ease, Fx};
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
    Key { pressed: bool, text: Option<char> },
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

/// The keyboard and mouse, as the scene shows them.
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

        let line_height = corrode_text::measure("> ", LINE_STYLE).height;
        let mut desk = Self {
            pointer,
            at: (screen.width as i32 / 2, screen.height as i32 / 2),
            buttons: 0,
            line: None,
            typed: String::new(),
            line_height,
            moved: false,
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
        // Anything that turned up since -- a window -- goes under the
        // pointer, not over it.
        if let Some([below, above]) = self.pointer {
            if scene.top() != Some(above) {
                scene.raise(below);
                scene.raise(above);
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
                    pressed: true,
                    text: Some(text),
                } => {
                    typed |= self.key(text);
                }
                Event::Key { .. } => {}
                Event::Pointer { dx, dy, buttons } => self.pointer(scene, now, dx, dy, buttons),
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
        let pressed = buttons & !self.buttons;
        if pressed & 1 != 0 {
            // What a click lands on, by the scene's own reckoning -- the
            // first thing a window manager will need to know.
            // The pointer is always on top and always under itself, so it
            // is looked past.
            let pointer = self.pointer.unwrap_or([LayerId::NONE; 2]);
            let under = scene
                .layers_at(self.at.0, self.at.1)
                .find(|id| !pointer.contains(id));
            println!(
                "input      : click at ({}, {}) on {}",
                self.at.0,
                self.at.1,
                if under.is_some() {
                    "a layer"
                } else {
                    "the background"
                }
            );
        }
        self.buttons = buttons;
    }
}
