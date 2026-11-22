//! Search: Alt+Space, as Spotlight is Cmd+Space and KRunner Alt+Space.
//!
//! A glass field near the top of the screen. What is typed narrows the
//! programs the dock knows to those whose names contain it, names that
//! start with it first, and below them the files whose names do. Up and
//! Down pick one; Return opens a program -- or brings its window forward,
//! if it is open -- or shows a file in Files; Escape puts the field away.
//!
//! Programs are shown with the dock's own icons and name stencils; files
//! with a document stencil and their names, drawn here with the typed line
//! and the magnifier.

use alloc::string::String;
use alloc::vec::Vec;

use corrode_render::anim::{Animation, Ease, Fx};
use corrode_render::scene::{Content, Layer, LayerId, Scene};
use corrode_render::{Color, Rect, Renderer};
use corrode_text::{Coverage, Style};
use corrode_theme::Theme;

use crate::dock::Dock;

/// Where the typed line and the magnifier keep their stencils: the right
/// half of the atlas, past the switcher's highlight.
const QUERY_SRC: Rect = Rect::new(600, crate::input::ATLAS_ROW as i32 + 152, 400, 28);
const GLASS_SRC: Rect = Rect::new(1000, crate::input::ATLAS_ROW as i32 + 152, 20, 20);
const QUERY: Style = Style::px(20);
/// A file's icon, and its name and folder, one slot a row below the query.
const DOC_AT: (i32, i32) = (600, crate::input::ATLAS_ROW as i32 + 180);
const FILE_NAME_AT: (i32, i32) = (680, crate::input::ATLAS_ROW as i32 + 180);
const FILE_NAME: (u32, u32) = (344, 19);
const FILE_STYLE: Style = Style::px(13);
/// At most this many files are listed: there is room for no more names.
const FILES: usize = 4;
const PLACEHOLDER: &str = "Search";

const WIDTH: u32 = 560;
const FIELD: u32 = 52;
/// From the top of the screen, as Spotlight sits.
const TOP: i32 = 150;
const PAD: i32 = 16;

/// One result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Hit {
    /// A program, as the dock's index for it.
    App(usize),
    /// A file, by its absolute path.
    File(String),
}

pub struct Spotlight {
    query: String,
    /// What matches, best first: programs, then files.
    matches: Vec<Hit>,
    /// The document stencil, if it was uploaded.
    doc: Option<Rect>,
    picked: usize,
    pane: LayerId,
    text: LayerId,
    caret: LayerId,
    /// Each result row's layers, and the pick's highlight.
    rows: Vec<LayerId>,
    highlight: Option<LayerId>,
    others: Vec<LayerId>,
    theme: Theme,
}

/// A sheet of paper with its top right corner folded, `size` square, the
/// sheet as wide as the dock's tiles are inset.
fn document(size: u32) -> Vec<u8> {
    let size = size as i32;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In quarters of a pixel.
    let (left, right) = (size * 4 * 24 / 100, size * 4 * 76 / 100);
    let (top, bottom) = (size * 4 * 10 / 100, size * 4 * 90 / 100);
    let fold = size * 4 * 16 / 100;
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px * 4 + sx, py * 4 + sy);
                    let sheet = x >= left && x < right && y >= top && y < bottom;
                    // The corner cut off along the diagonal.
                    let cut = (right - x) + (y - top) < fold;
                    hits += (sheet && !cut) as i32;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

/// A magnifying glass: a ring and a handle to the lower right.
fn magnifier() -> Vec<u8> {
    let size = GLASS_SRC.width as i32;
    let mut coverage = alloc::vec![0u8; (size * size) as usize];
    // In quarters of a pixel. The ring's centre and radii; the handle runs
    // from the ring out to the corner.
    let (cx, cy, outer, inner) = (32, 32, 28, 20);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px * 4 + sx, py * 4 + sy);
                    let d = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                    let ring = d <= outer * outer && d >= inner * inner;
                    // Along the diagonal, within a pixel and a half of it.
                    let along = x + y - (cx + cy);
                    let handle = (x - y).abs() <= 8 && along >= 50 && along <= 100;
                    hits += (ring || handle) as i32;
                }
            }
            coverage[(py * size + px) as usize] = (hits * 255 / 16) as u8;
        }
    }
    coverage
}

impl Spotlight {
    /// Put the field up, empty, with every program listed. None without a
    /// dock, whose programs are what it searches.
    pub fn open<R: Renderer, const N: usize>(
        renderer: &mut R,
        scene: &mut Scene<N>,
        theme: &Theme,
        dock: Option<&Dock>,
        now: u64,
    ) -> Option<Self> {
        dock?;
        let x = (scene.screen().width as i32 - WIDTH as i32) / 2;
        let mut pane = crate::input::pane(Rect::new(x, TOP, WIDTH, FIELD), 16, theme);
        let ms = theme.ms(140);
        if ms > 0 {
            pane.opacity = Animation::ease(now, ms, Fx::ZERO, Fx::from_int(255), Ease::Out);
        }
        let pane = scene.add(pane)?;
        let mut others = Vec::new();
        if renderer.upload_mask(GLASS_SRC, &magnifier()).is_ok() {
            let y = TOP + (FIELD as i32 - GLASS_SRC.height as i32) / 2;
            let mut glass = Layer::mask(x + PAD, y, GLASS_SRC, theme.text());
            glass.opacity = Animation::rest(Fx::from_int(150));
            others.extend(scene.add(glass));
        }
        let text_x = x + PAD + GLASS_SRC.width as i32 + 12;
        let text_y = TOP + (FIELD as i32 - QUERY_SRC.height as i32) / 2;
        let text = scene.add(Layer::mask(text_x, text_y, QUERY_SRC, theme.text()))?;
        // Only as big as the room for it; files go without icons past that.
        let size = dock.map_or(0, |dock| dock.size());
        let doc_src = Rect::new(DOC_AT.0, DOC_AT.1, size, size);
        let doc = (size <= 72 && renderer.upload_mask(doc_src, &document(size)).is_ok())
            .then_some(doc_src);
        let caret = scene.add(Layer::solid(
            Rect::new(text_x, text_y + 3, 2, QUERY_SRC.height - 6),
            theme.accent,
        ))?;
        let mut spotlight = Spotlight {
            query: String::new(),
            matches: Vec::new(),
            doc,
            picked: 0,
            pane,
            text,
            caret,
            rows: Vec::new(),
            highlight: None,
            others,
            theme: *theme,
        };
        spotlight.refresh(renderer, scene, dock);
        Some(spotlight)
    }

    /// A key that types `text`, or Backspace (`None`): the query changes,
    /// and with it the results.
    pub fn typed<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        dock: Option<&Dock>,
        text: Option<char>,
    ) {
        match text {
            Some(c) if !c.is_control() => {
                if corrode_text::measure(&self.query, QUERY).width + 24 < QUERY_SRC.width {
                    self.query.push(c);
                }
            }
            Some(_) => return,
            None => {
                self.query.pop();
            }
        }
        self.picked = 0;
        self.refresh(renderer, scene, dock);
    }

    /// Move the pick down, or up for `up`, stopping at the ends.
    pub fn pick<const N: usize>(&mut self, scene: &mut Scene<N>, up: bool) {
        let last = self.matches.len().saturating_sub(1);
        self.picked = if up {
            self.picked.saturating_sub(1)
        } else {
            (self.picked + 1).min(last)
        };
        self.place_highlight(scene);
    }

    /// What is picked.
    pub fn picked(&self) -> Option<Hit> {
        self.matches.get(self.picked).cloned()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Draw the query, match the programs and lay out their rows.
    fn refresh<R: Renderer, const N: usize>(
        &mut self,
        renderer: &mut R,
        scene: &mut Scene<N>,
        dock: Option<&Dock>,
    ) {
        let Some(dock) = dock else { return };
        // The line, or the placeholder, faint, when nothing is typed.
        let (shown, opacity) = if self.query.is_empty() {
            (PLACEHOLDER, 110)
        } else {
            (self.query.as_str(), 255)
        };
        let (width, height) = (QUERY_SRC.width, QUERY_SRC.height);
        let mut coverage = alloc::vec![0u8; (width * height) as usize];
        corrode_text::draw(shown, QUERY, &mut Coverage::new(&mut coverage, width as usize));
        let extent = corrode_text::measure(shown, QUERY);
        if renderer.upload_mask(QUERY_SRC, &coverage).is_ok() {
            if let Some(layer) = scene.layer_mut(self.text) {
                layer.width = Animation::rest(Fx::from_int(extent.width.min(width) as i32));
                layer.opacity = Animation::rest(Fx::from_int(opacity));
                layer.content = Content::Mask { src: QUERY_SRC };
            }
            scene.damage(self.text, Rect::new(0, 0, width, height));
        }
        let typed = if self.query.is_empty() { 0 } else { extent.width as i32 };
        let text_x = scene.layer(self.text).map_or(0, |layer| layer.x.sample(0).value.round());
        if let Some(caret) = scene.layer_mut(self.caret) {
            caret.x = Animation::rest(Fx::from_int(text_x + typed + 1));
        }

        // Names that start with the query first, then those containing it.
        let query = self.query.to_ascii_lowercase();
        let mut starts = Vec::new();
        let mut contains = Vec::new();
        for index in 0..dock.apps() {
            let name = dock.title(index).to_ascii_lowercase();
            if name.starts_with(&query) {
                starts.push(index);
            } else if name.contains(&query) {
                contains.push(index);
            }
        }
        starts.extend(contains);
        self.matches = starts.into_iter().map(Hit::App).collect();
        // Files, by name, once something is typed.
        if !query.is_empty() {
            let archive = crate::vault::root().map_or(&[][..], |(_, module)| module.bytes);
            let mut starts = Vec::new();
            let mut contains = Vec::new();
            for (path, _) in corrode_theme::tar_files(archive) {
                let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
                let found = alloc::format!("/{path}");
                if name.starts_with(&query) {
                    starts.push(found);
                } else if name.contains(&query) {
                    contains.push(found);
                }
            }
            starts.extend(contains);
            self.matches
                .extend(starts.into_iter().take(FILES).map(Hit::File));
        }

        // The rows, built again: there are few of them.
        for id in self.rows.drain(..) {
            scene.remove(id);
        }
        if let Some(id) = self.highlight.take() {
            scene.remove(id);
        }
        let size = dock.size();
        let row = size + 12;
        let x = scene.layer(self.pane).map_or(0, |pane| pane.x.sample(0).value.round());
        let height = FIELD + if self.matches.is_empty() { 0 } else { self.matches.len() as u32 * row + 12 };
        if let Some(pane) = scene.layer_mut(self.pane) {
            pane.height = Animation::rest(Fx::from_int(height as i32));
        }
        if !self.matches.is_empty() {
            let line = Layer::solid(Rect::new(x, TOP + FIELD as i32, WIDTH, 1), self.theme.text());
            if let Some(id) = scene.add(line) {
                if let Some(layer) = scene.layer_mut(id) {
                    layer.opacity = Animation::rest(Fx::from_int(40));
                }
                self.rows.push(id);
            }
            let mut highlight = Layer::solid(Rect::new(x + 8, 0, WIDTH - 16, row), self.theme.accent);
            highlight.opacity = Animation::rest(Fx::from_int(150));
            self.highlight = scene.add(highlight);
        }
        let mut files = 0;
        let matches = self.matches.clone();
        for (slot, hit) in matches.iter().enumerate() {
            let y = TOP + FIELD as i32 + 6 + (slot as u32 * row) as i32 + 6;
            let app = match hit {
                Hit::App(app) => *app,
                Hit::File(path) => {
                    if let Some(doc) = self.doc {
                        let sheet = Layer::mask(x + PAD, y, doc, Color::rgb(0xf4, 0xf4, 0xf6));
                        self.rows.extend(scene.add(sheet));
                    }
                    // "motd  /etc": the name, then where it is.
                    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
                    let folder = if folder.is_empty() { "/" } else { folder };
                    let text = alloc::format!("{name}   {folder}");
                    let src = Rect::new(
                        FILE_NAME_AT.0,
                        FILE_NAME_AT.1 + files * FILE_NAME.1 as i32,
                        FILE_NAME.0,
                        FILE_NAME.1,
                    );
                    files += 1;
                    let extent = corrode_text::measure(&text, FILE_STYLE);
                    let mut coverage = alloc::vec![0u8; (src.width * src.height) as usize];
                    corrode_text::draw(&text, FILE_STYLE, &mut Coverage::new(&mut coverage, src.width as usize));
                    if renderer.upload_mask(src, &coverage).is_ok() {
                        let label_y = y + (size as i32 - extent.height as i32) / 2;
                        let mut label =
                            Layer::mask(x + PAD + size as i32 + 14, label_y, src, self.theme.text());
                        label.width = Animation::rest(Fx::from_int(extent.width.min(src.width) as i32));
                        self.rows.extend(scene.add(label));
                    }
                    continue;
                }
            };
            if let Some((tile, glyph, color)) = dock.stencils(app) {
                self.rows.extend(scene.add(Layer::mask(x + PAD, y, tile, color)));
                if let Some(glyph) = glyph {
                    self.rows.extend(scene.add(Layer::mask(x + PAD, y, glyph, Color::WHITE)));
                }
            }
            if let Some(label) = dock.label(scene, app) {
                let label_y = y + (size as i32 - label.height as i32) / 2;
                let name = Layer::mask(x + PAD + size as i32 + 14, label_y, label, self.theme.text());
                self.rows.extend(scene.add(name));
            }
        }
        self.place_highlight(scene);
    }

    fn place_highlight<const N: usize>(&mut self, scene: &mut Scene<N>) {
        let Some(id) = self.highlight else { return };
        let Some(layer) = scene.layer_mut(id) else { return };
        let row = layer.height.sample(0).value.round();
        let y = TOP + FIELD as i32 + 6 + self.picked as i32 * row;
        layer.y = Animation::rest(Fx::from_int(y));
        // The highlight sits under the rows it lights: the rows go above.
        let rows = self.rows.clone();
        for id in rows {
            scene.raise(id);
        }
    }

    /// Every layer, bottom first, to keep it above the windows.
    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        [self.pane]
            .into_iter()
            .chain(self.others.iter().copied())
            .chain([self.text, self.caret])
            .chain(self.highlight)
            .chain(self.rows.iter().copied())
    }

    /// Take the field down.
    pub fn close<const N: usize>(self, scene: &mut Scene<N>) {
        let ids: Vec<LayerId> = self.layers().collect();
        for id in ids {
            scene.remove(id);
        }
    }
}
