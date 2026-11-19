//! The dock: a pane of glass floating above the bottom of the screen, with
//! an icon for each program the launcher knows.
//!
//! Clicking an icon asks the launcher to start the program -- through the
//! same endpoint the panel holds, so the dock can start nothing the panel
//! could not -- and the icon jumps while it does, on a spring. A dot under
//! an icon says the program has a window open.
//!
//! The icons are drawn here, once, as stencils in the mask atlas: a rounded
//! square in the program's colour (a superellipse, as Apple's icons are)
//! with a white glyph over it.

use alloc::vec::Vec;

use corrode_abi::launcher;
use corrode_render::anim::{Animation, Fx, Spring, Velocity};
use corrode_render::scene::{Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_text::{Coverage, Style, Weight};
use corrode_theme::Theme;

use crate::println;

/// What a dock icon looks like.
#[derive(Clone, Copy)]
enum Glyph {
    /// A line of text, centred.
    Text(&'static str),
    Gear,
}

/// A program the dock can start.
struct App {
    /// What its window's title says, to know when it is running.
    title: &'static str,
    program: u64,
    color: Color,
    glyph: Glyph,
}

const APPS: [App; 3] = [
    App {
        title: "Terminal",
        program: launcher::TERMINAL,
        color: Color::rgb(0x1d, 0x1f, 0x24),
        glyph: Glyph::Text(">_"),
    },
    App {
        title: "Settings",
        program: launcher::SETTINGS,
        color: Color::rgb(0x7c, 0x7f, 0x88),
        glyph: Glyph::Gear,
    },
    App {
        title: "About",
        program: launcher::ABOUT,
        color: Color::rgb(0x0a, 0x84, 0xff),
        glyph: Glyph::Text("i"),
    },
];

/// Where the dock keeps its stencils: the right half of the atlas, below
/// the pointer's row. The tile, then each glyph, side by side; the running
/// dot below them.
const ATLAS_X: i32 = 512;
const ATLAS_Y: i32 = crate::input::ATLAS_ROW as i32 + 64;
const DOT: u32 = 5;

/// The dock floats this far above the bottom edge, with this much around
/// its icons.
const GAP: i32 = 10;
const PAD: u32 = 8;

struct Icon {
    tile: LayerId,
    glyph: Option<LayerId>,
    dot: Option<LayerId>,
    /// Where the icon rests.
    at: (i32, i32),
    running: bool,
}

pub struct Dock {
    pane: LayerId,
    icons: Vec<Icon>,
    size: u32,
    bounce: bool,
}

/// How much of each pixel of a `size`-pixel superellipse tile is covered:
/// `|x|^5 + |y|^5 <= r^5`, sampled four by four.
fn tile(size: u32) -> Vec<u8> {
    let size = size as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In eighths of a pixel from the centre.
    let r = size * 4 - 2;
    let r5 = r.pow(5);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (px * 8 + sx * 2 + 1 - size * 4).abs();
                    let y = (py * 8 + sy * 2 + 1 - size * 4).abs();
                    hits += (x.pow(5) + y.pow(5) <= r5) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// A cog with eight teeth and a hole, `size` pixels across.
fn gear(size: u32) -> Vec<u8> {
    let size = size as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // Radii in eighths of a pixel: the body, the teeth's tips, the hole.
    let (body, tip, hole) = (size * 8 * 24 / 100, size * 8 * 33 / 100, size * 8 * 10 / 100);
    let tooth = size * 8 * 6 / 100;
    // The eight directions, in 4096ths.
    const DIRS: [(i64, i64); 4] = [(4096, 0), (2896, 2896), (0, 4096), (-2896, 2896)];
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px * 8 + sx * 2 + 1 - size * 4;
                    let y = py * 8 + sy * 2 + 1 - size * 4;
                    let d2 = x * x + y * y;
                    let in_tooth = d2 <= tip * tip
                        && DIRS
                            .iter()
                            .any(|(dx, dy)| (dx * y - dy * x).abs() / 4096 <= tooth);
                    let solid = (d2 <= body * body || in_tooth) && d2 > hole * hole;
                    hits += solid as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// Text centred in a `size`-pixel square.
fn text(size: u32, text: &str) -> Vec<u8> {
    let style = Style::px((size * 52 / 100).max(8)).weight(Weight::Bold);
    let extent = corrode_text::measure(text, style);
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    if extent.width > size || extent.height > size {
        return coverage;
    }
    let mut drawn = alloc::vec![0u8; extent.area()];
    corrode_text::draw(text, style, &mut Coverage::new(&mut drawn, extent.width as usize));
    let (left, top) = ((size - extent.width) / 2, (size - extent.height) / 2);
    for y in 0..extent.height {
        for x in 0..extent.width {
            coverage[((top + y) * size + left + x) as usize] =
                drawn[(y * extent.width + x) as usize];
        }
    }
    coverage
}

/// A round dot, `DOT` pixels across.
fn dot() -> Vec<u8> {
    let size = DOT as i64;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    let r = size * 4;
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = px * 8 + sx * 2 + 1 - size * 4;
                    let y = py * 8 + sy * 2 + 1 - size * 4;
                    hits += (x * x + y * y <= r * r) as i64;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

impl Dock {
    /// Draw the icons into the atlas and put the dock in the scene, or
    /// `None` if the theme has no dock or there is no room for one.
    pub fn new<R: Renderer, const N: usize>(
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: &Theme,
    ) -> Option<Self> {
        if !theme.dock {
            return None;
        }
        let size = theme.dock_size as u32;
        let screen = scene.screen();
        let count = APPS.len() as u32;
        let (width, height) = (count * (size + PAD) + PAD, size + 2 * PAD);
        let left = (screen.width as i32 - width as i32) / 2;
        let top = screen.height as i32 - height as i32 - GAP;
        let pane = scene.add(crate::input::pane(
            Rect::new(left, top, width, height),
            (height / 2 - 4) as u16,
            theme,
        ))?;

        let tile_src = Rect::new(ATLAS_X, ATLAS_Y, size, size);
        let dot_src = Rect::new(ATLAS_X, ATLAS_Y + size as i32 + 2, DOT, DOT);
        let stencils = renderer.upload_mask(tile_src, &tile(size)).is_ok()
            && renderer.upload_mask(dot_src, &dot()).is_ok();

        let mut icons = Vec::new();
        for (index, app) in APPS.iter().enumerate() {
            let at = (left + (PAD + index as u32 * (size + PAD)) as i32, top + PAD as i32);
            let Some(tile) = stencils
                .then(|| scene.add(Layer::mask(at.0, at.1, tile_src, app.color)))
                .flatten()
            else {
                // Without stencils, a plain square says as much.
                let tile = scene.add(Layer::solid(Rect::new(at.0, at.1, size, size), app.color))?;
                icons.push(Icon {
                    tile,
                    glyph: None,
                    dot: None,
                    at,
                    running: false,
                });
                continue;
            };
            let glyph_src = Rect::new(
                ATLAS_X + (index as i32 + 1) * size as i32,
                ATLAS_Y,
                size,
                size,
            );
            let coverage = match app.glyph {
                Glyph::Text(label) => text(size, label),
                Glyph::Gear => gear(size),
            };
            let glyph = renderer
                .upload_mask(glyph_src, &coverage)
                .ok()
                .and_then(|_| scene.add(Layer::mask(at.0, at.1, glyph_src, Color::WHITE)));
            let mut dot = Layer::mask(
                at.0 + (size - DOT) as i32 / 2,
                top + height as i32 - DOT as i32 - 1,
                dot_src,
                theme.text(),
            );
            dot.opacity = Animation::rest(Fx::ZERO);
            let dot = scene.add(dot);
            icons.push(Icon {
                tile,
                glyph,
                dot,
                at,
                running: false,
            });
        }
        Some(Dock {
            pane,
            icons,
            size,
            bounce: theme.bounce && theme.speed > 0,
        })
    }

    /// Every layer of the dock, bottom first.
    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        core::iter::once(self.pane).chain(
            self.icons
                .iter()
                .flat_map(|icon| [Some(icon.tile), icon.glyph, icon.dot].into_iter().flatten()),
        )
    }

    /// Which icon `id` is part of, if any. The pane itself is not an icon.
    pub fn icon(&self, id: LayerId) -> Option<usize> {
        self.icons
            .iter()
            .position(|icon| icon.tile == id || icon.glyph == Some(id))
    }

    /// What icon `index`'s program titles its windows.
    pub fn title(&self, index: usize) -> &'static str {
        APPS.get(index).map_or("", |app| app.title)
    }

    /// Whether `id` is the dock at all.
    pub fn contains(&self, id: LayerId) -> bool {
        self.layers().any(|layer| layer == id)
    }

    /// Start icon `index`'s program: jump, and ask the launcher.
    pub fn launch<const N: usize>(&self, index: usize, scene: &mut Scene<N>, now: u64) {
        let (Some(icon), Some(app)) = (self.icons.get(index), APPS.get(index)) else {
            return;
        };
        if self.bounce {
            let height = (self.size * 45 / 100) as i32;
            for id in [Some(icon.tile), icon.glyph].into_iter().flatten() {
                if let Some(layer) = scene.layer_mut(id) {
                    layer.y = Animation::spring(
                        now,
                        Fx::from_int(icon.at.1 - height),
                        Fx::from_int(icon.at.1),
                        Velocity::ZERO,
                        Spring::BOUNCY,
                    );
                }
            }
        }
        let answer = crate::ipc::call(
            crate::LAUNCHER_ENDPOINT,
            crate::ipc::Message {
                label: launcher::LAUNCH,
                words: [app.program, 0, 0, 0],
            },
            None,
            None,
        );
        match answer {
            Ok(answer) if answer.message.words[0] == 0 => {
                println!("dock       : started {}", app.title)
            }
            Ok(answer) => println!(
                "dock       : the launcher would not start {} ({})",
                app.title, answer.message.words[0]
            ),
            Err(error) => println!("dock       : no launcher: {error:?}"),
        }
    }

    /// Light the dots of the programs that have a window: the titles of
    /// every window there is.
    pub fn running<'a, const N: usize>(
        &mut self,
        scene: &mut Scene<N>,
        titles: impl Iterator<Item = &'a str> + Clone,
    ) {
        for (icon, app) in self.icons.iter_mut().zip(APPS.iter()) {
            let running = titles.clone().any(|title| title == app.title);
            if running == icon.running {
                continue;
            }
            icon.running = running;
            if let Some(layer) = icon.dot.and_then(|id| scene.layer_mut(id)) {
                layer.opacity = Animation::rest(Fx::from_int(if running { 255 } else { 0 }));
            }
        }
    }
}
