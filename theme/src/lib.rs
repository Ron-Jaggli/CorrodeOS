//! The desktop's look, as one file anybody can edit: `/etc/desktop.conf`.
//!
//! ```text
//! # Start from a preset, then change what you like.
//! preset     = corrode
//! appearance = dark
//! accent     = #e2643a
//! glass      = regular
//! ```
//!
//! One `key = value` a line; `#` starts a comment; unknown keys and values
//! that do not parse are skipped, so a typo costs that one line and never
//! the desktop. `preset` is applied first wherever it is in the file, and
//! every other line on top of it.
//!
//! The kernel reads the file at boot from the files packed into the ISO,
//! and tells each program the result in four words when it opens a window
//! ([`Theme::to_words`]), so nothing but the kernel ever parses it.

#![cfg_attr(not(test), no_std)]

pub mod clock;
pub mod wallpaper;

pub use corrode_render::Color;
use corrode_render::glass::{Glass, Material};

/// What the desktop's panes are made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlassMode {
    Regular,
    Clear,
    /// Solid panes, for a slow machine or a plainer taste.
    Off,
}

/// Which side of the title bar the window buttons go: macOS's or Plasma's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buttons {
    Left,
    Right,
}

/// What is painted behind everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wallpaper {
    /// Soft glowing blobs of the three colours, like a Tahoe wallpaper.
    Aurora,
    /// Overlapping diagonal bands, after Plasma's Breeze wallpapers.
    Ribbons,
    /// Rolling layered hills, darkening towards the bottom.
    Dunes,
    /// A plain vertical gradient from the first colour to the second.
    Gradient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub dark: bool,
    /// Focused title bars, highlights, the pressed button.
    pub accent: Color,
    pub glass: GlassMode,
    /// How far the glass bends what is behind it at the rim; 0 to 120.
    pub refraction: u8,
    /// How frosted Regular glass is, as a percentage; 0 to 200.
    pub frost: u8,
    /// Tint for every pane, and how much of it out of 255.
    pub tint: Color,
    pub tint_amount: u8,
    /// Window corner radius, in pixels.
    pub radius: u8,
    pub buttons: Buttons,
    pub wallpaper: Wallpaper,
    pub wallpaper_colors: [Color; 3],
    pub dock: bool,
    /// Dock icon size, in pixels.
    pub dock_size: u8,
    /// Whether dock icons jump when they start something.
    pub bounce: bool,
    /// How big a dock icon grows under the pointer, in percent of its
    /// size: 100 for not at all, up to 200.
    pub magnify: u8,
    pub shadows: bool,
    /// Animation speed as a percentage: 100 normal, 0 no animation.
    pub speed: u8,
    /// The menu bar clock's offset from UTC, in minutes.
    pub utc_offset: i16,
    /// 24-hour clock, or 12-hour with AM and PM.
    pub clock_24h: bool,
}

/// What a kept theme starts with, and how long one is. The last byte is the
/// format's version.
pub const RECORD_MAGIC: &[u8; 8] = b"CORRODE\x01";
pub const RECORD_LEN: usize = 48;

/// FNV-1a, 64-bit.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The presets, by name. The first is the default.
pub const PRESETS: [(&str, Theme); 6] = [
    ("corrode", CORRODE),
    ("tahoe", TAHOE),
    ("tahoe-light", TAHOE_LIGHT),
    ("breeze", BREEZE),
    ("crystal", CRYSTAL),
    ("plain", PLAIN),
];

/// macOS's glass, dock and traffic lights; Plasma's colours and its
/// launcher at the left of the panel; rust, because it is Corrode.
pub const CORRODE: Theme = Theme {
    dark: true,
    accent: Color::rgb(0xe2, 0x64, 0x3a),
    glass: GlassMode::Regular,
    refraction: 60,
    frost: 100,
    tint: Color::rgb(0xff, 0xff, 0xff),
    tint_amount: 0,
    radius: 12,
    buttons: Buttons::Left,
    wallpaper: Wallpaper::Aurora,
    wallpaper_colors: [
        Color::rgb(0x2a, 0x1b, 0x4a),
        Color::rgb(0xd9, 0x5a, 0x2b),
        Color::rgb(0x1d, 0x6f, 0x8c),
    ],
    dock: true,
    dock_size: 48,
    bounce: true,
    magnify: 140,
    shadows: true,
    speed: 100,
    utc_offset: 0,
    clock_24h: true,
};

pub const TAHOE: Theme = Theme {
    accent: Color::rgb(0x0a, 0x84, 0xff),
    wallpaper_colors: [
        Color::rgb(0x10, 0x2a, 0x6b),
        Color::rgb(0x7b, 0x3f, 0xd1),
        Color::rgb(0xf0, 0x8a, 0x5d),
    ],
    radius: 14,
    ..CORRODE
};

pub const TAHOE_LIGHT: Theme = Theme {
    dark: false,
    wallpaper_colors: [
        Color::rgb(0x8e, 0xc5, 0xff),
        Color::rgb(0xf6, 0xb2, 0xd9),
        Color::rgb(0xff, 0xe0, 0xa8),
    ],
    ..TAHOE
};

pub const BREEZE: Theme = Theme {
    accent: Color::rgb(0x3d, 0xae, 0xe9),
    buttons: Buttons::Right,
    radius: 8,
    wallpaper: Wallpaper::Ribbons,
    wallpaper_colors: [
        Color::rgb(0x14, 0x1e, 0x2e),
        Color::rgb(0x1d, 0x99, 0xf3),
        Color::rgb(0x8e, 0x44, 0xad),
    ],
    frost: 140,
    bounce: false,
    // Plasma's panel does not magnify.
    magnify: 100,
    ..CORRODE
};

/// Clear glass, bent hard, to show what it does.
pub const CRYSTAL: Theme = Theme {
    glass: GlassMode::Clear,
    refraction: 110,
    wallpaper: Wallpaper::Dunes,
    wallpaper_colors: [
        Color::rgb(0x0b, 0x13, 0x2b),
        Color::rgb(0x3a, 0x86, 0xff),
        Color::rgb(0xff, 0xbe, 0x0b),
    ],
    ..TAHOE
};

/// No glass, no shadows, no animation: the cheapest desktop to draw.
pub const PLAIN: Theme = Theme {
    glass: GlassMode::Off,
    shadows: false,
    bounce: false,
    magnify: 100,
    speed: 0,
    wallpaper: Wallpaper::Gradient,
    wallpaper_colors: [
        Color::rgb(0x1c, 0x1f, 0x26),
        Color::rgb(0x2b, 0x2f, 0x38),
        Color::rgb(0x2b, 0x2f, 0x38),
    ],
    ..CORRODE
};

impl Default for Theme {
    fn default() -> Self {
        CORRODE
    }
}

impl Theme {
    /// The theme a config file describes, over the default.
    pub fn parse(text: &str) -> Theme {
        let lines = || {
            text.lines().filter_map(|line| {
                let line = uncommented(line).trim();
                let (key, value) = line.split_once('=')?;
                Some((key.trim(), value.trim()))
            })
        };
        let mut theme = lines()
            .filter(|(key, _)| *key == "preset")
            .filter_map(|(_, value)| preset(value))
            .last()
            .unwrap_or_default();
        for (key, value) in lines() {
            theme.set(key, value);
        }
        theme
    }

    /// Change one setting, as a config line would. False if the key is
    /// unknown or the value does not parse; nothing changes then.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        let number = |most: u32| value.parse::<u32>().ok().map(|n| n.min(most) as u8);
        let on = || match value {
            "on" | "yes" | "true" => Some(true),
            "off" | "no" | "false" => Some(false),
            _ => None,
        };
        match key {
            "preset" => return preset(value).is_some(),
            "appearance" => match value {
                "dark" => self.dark = true,
                "light" => self.dark = false,
                _ => return false,
            },
            "accent" => match color(value) {
                Some(c) => self.accent = c,
                None => return false,
            },
            "glass" => match value {
                "regular" => self.glass = GlassMode::Regular,
                "clear" => self.glass = GlassMode::Clear,
                "off" => self.glass = GlassMode::Off,
                _ => return false,
            },
            "refraction" => match number(120) {
                Some(n) => self.refraction = n,
                None => return false,
            },
            "frost" => match number(200) {
                Some(n) => self.frost = n,
                None => return false,
            },
            "tint" => match value {
                "none" => self.tint_amount = 0,
                _ => match color(value) {
                    Some(c) => {
                        self.tint = c;
                        if self.tint_amount == 0 {
                            self.tint_amount = 64;
                        }
                    }
                    None => return false,
                },
            },
            "tint_amount" => match number(255) {
                Some(n) => self.tint_amount = n,
                None => return false,
            },
            "corner_radius" => match number(32) {
                Some(n) => self.radius = n,
                None => return false,
            },
            "buttons" => match value {
                "left" => self.buttons = Buttons::Left,
                "right" => self.buttons = Buttons::Right,
                _ => return false,
            },
            "wallpaper" => match value {
                "aurora" => self.wallpaper = Wallpaper::Aurora,
                "ribbons" => self.wallpaper = Wallpaper::Ribbons,
                "dunes" => self.wallpaper = Wallpaper::Dunes,
                "gradient" => self.wallpaper = Wallpaper::Gradient,
                _ => return false,
            },
            "wallpaper_colors" => {
                let mut colors = self.wallpaper_colors;
                let mut found = 0;
                for (slot, word) in colors.iter_mut().zip(value.split_whitespace()) {
                    match color(word) {
                        Some(c) => *slot = c,
                        None => return false,
                    }
                    found += 1;
                }
                if found == 0 {
                    return false;
                }
                self.wallpaper_colors = colors;
            }
            "dock" => match on() {
                Some(b) => self.dock = b,
                None => return false,
            },
            "dock_size" => match number(96) {
                Some(n) => self.dock_size = n.max(24),
                None => return false,
            },
            "bounce" => match on() {
                Some(b) => self.bounce = b,
                None => return false,
            },
            // on, off, or how big in percent.
            "magnify" => match (on(), number(200)) {
                (Some(b), _) => self.magnify = if b { CORRODE.magnify } else { 100 },
                (None, Some(n)) => self.magnify = n.max(100),
                (None, None) => return false,
            },
            "shadows" => match on() {
                Some(b) => self.shadows = b,
                None => return false,
            },
            "animation_speed" => match number(200) {
                Some(n) => self.speed = n,
                None => return false,
            },
            "timezone" => match offset(value) {
                Some(minutes) => self.utc_offset = minutes,
                None => return false,
            },
            "clock" => match value {
                "24h" => self.clock_24h = true,
                "12h" => self.clock_24h = false,
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    /// The glass the desktop's panes are made of, at a corner radius, or
    /// `None` when glass is off.
    pub fn pane(&self, radius: u16) -> Option<Glass> {
        let material = match self.glass {
            GlassMode::Regular => Material::Regular,
            GlassMode::Clear => Material::Clear,
            GlassMode::Off => return None,
        };
        Some(Glass {
            material,
            dark: self.dark,
            radius,
            refraction: self.refraction,
            frost: self.frost,
            tint: self.tint,
            tint_amount: self.tint_amount,
            rim: true,
        })
    }

    /// What a pane is when glass is off: a solid colour.
    pub fn surface(&self) -> Color {
        if self.dark {
            Color::rgb(0x24, 0x26, 0x2c)
        } else {
            Color::rgb(0xec, 0xec, 0xf0)
        }
    }

    /// Text on the desktop's panes.
    pub fn text(&self) -> Color {
        if self.dark {
            Color::rgb(0xf4, 0xf1, 0xec)
        } else {
            Color::rgb(0x1d, 0x1d, 0x1f)
        }
    }

    /// Scale a duration by the animation speed: zero means none.
    pub fn ms(&self, ms: u32) -> u32 {
        match self.speed {
            0 => 0,
            speed => ms * 100 / speed as u32,
        }
    }

    /// The theme as it is kept on a disk: [`RECORD_MAGIC`], the four words of
    /// [`Theme::to_words`], and an FNV-1a hash of those forty bytes, all
    /// little-endian.
    pub fn to_record(&self) -> [u8; RECORD_LEN] {
        let mut out = [0u8; RECORD_LEN];
        out[..8].copy_from_slice(RECORD_MAGIC);
        for (index, word) in self.to_words().iter().enumerate() {
            out[8 + index * 8..16 + index * 8].copy_from_slice(&word.to_le_bytes());
        }
        let hash = fnv(&out[..40]);
        out[40..48].copy_from_slice(&hash.to_le_bytes());
        out
    }

    /// A theme kept by [`Theme::to_record`], or `None` if `bytes` are not
    /// one: a blank disk, someone else's, or one torn mid-write.
    pub fn from_record(bytes: &[u8]) -> Option<Theme> {
        let bytes = bytes.get(..RECORD_LEN)?;
        if &bytes[..8] != RECORD_MAGIC {
            return None;
        }
        let word = |at: usize| {
            let mut le = [0u8; 8];
            le.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(le)
        };
        if word(40) != fnv(&bytes[..40]) {
            return None;
        }
        Some(Theme::from_words([word(8), word(16), word(24), word(32)]))
    }

    /// Everything, in four words, for a program to be told in a message.
    pub fn to_words(&self) -> [u64; 4] {
        let rgb = |c: Color| (c.r as u64) << 16 | (c.g as u64) << 8 | c.b as u64;
        let flags = self.dark as u64
            | (self.glass as u64) << 1
            | (self.buttons as u64) << 3
            | (self.wallpaper as u64) << 4
            | (self.dock as u64) << 7
            | (self.bounce as u64) << 8
            | (self.shadows as u64) << 9;
        [
            flags
                | (self.refraction as u64) << 16
                | (self.frost as u64) << 24
                | (self.radius as u64) << 32
                | (self.dock_size as u64) << 40
                | (self.speed as u64) << 48
                | (self.tint_amount as u64) << 56,
            rgb(self.accent) | rgb(self.tint) << 24,
            rgb(self.wallpaper_colors[0]) | rgb(self.wallpaper_colors[1]) << 24,
            rgb(self.wallpaper_colors[2])
                | (self.utc_offset as u16 as u64) << 24
                | (!self.clock_24h as u64) << 40
                | (self.magnify as u64) << 48,
        ]
    }

    /// The other half of [`Theme::to_words`]. Any four words are some theme.
    pub fn from_words(words: [u64; 4]) -> Theme {
        let rgb = |w: u64| Color::rgb((w >> 16) as u8, (w >> 8) as u8, w as u8);
        let byte = |shift: u32| (words[0] >> shift) as u8;
        let flags = words[0];
        Theme {
            dark: flags & 1 != 0,
            glass: match (flags >> 1) & 3 {
                0 => GlassMode::Regular,
                1 => GlassMode::Clear,
                _ => GlassMode::Off,
            },
            buttons: if (flags >> 3) & 1 == 0 {
                Buttons::Left
            } else {
                Buttons::Right
            },
            wallpaper: match (flags >> 4) & 7 {
                0 => Wallpaper::Aurora,
                1 => Wallpaper::Ribbons,
                2 => Wallpaper::Dunes,
                _ => Wallpaper::Gradient,
            },
            dock: flags & 1 << 7 != 0,
            bounce: flags & 1 << 8 != 0,
            shadows: flags & 1 << 9 != 0,
            refraction: byte(16),
            frost: byte(24),
            radius: byte(32),
            dock_size: byte(40),
            speed: byte(48),
            tint_amount: byte(56),
            accent: rgb(words[1]),
            tint: rgb(words[1] >> 24),
            wallpaper_colors: [rgb(words[2]), rgb(words[2] >> 24), rgb(words[3])],
            utc_offset: (words[3] >> 24) as u16 as i16,
            clock_24h: words[3] & 1 << 40 == 0,
            magnify: ((words[3] >> 48) as u8).clamp(100, 200),
        }
    }
}

/// A line without its comment. A comment is a `#` that starts the line, or
/// one with space before it and space or nothing after -- so `#e2643a` is a
/// colour and not a comment.
fn uncommented(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        let before = at == 0 || bytes[at - 1].is_ascii_whitespace();
        let after = bytes.get(at + 1).is_none_or(|b| b.is_ascii_whitespace());
        if *byte == b'#' && before && (at == 0 || after) {
            return &line[..at];
        }
    }
    line
}

/// A preset by name.
/// A UTC offset in minutes from `UTC`, `+5`, `-8`, `+05:30` or `-3:30`:
/// within fourteen hours either way, as every real one is.
pub fn offset(text: &str) -> Option<i16> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("utc") || text == "0" {
        return Some(0);
    }
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None => (rest, "0"),
    };
    let (hours, minutes): (i16, i16) = (hours.parse().ok()?, minutes.parse().ok()?);
    if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

pub fn preset(name: &str) -> Option<Theme> {
    PRESETS
        .iter()
        .find(|(preset, _)| *preset == name)
        .map(|(_, theme)| *theme)
}

/// `#rrggbb`.
pub fn color(text: &str) -> Option<Color> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some(Color::rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// The lock screen's passcode, from a config file's `passcode = ` line: 4
/// to 8 digits, as ASCII, NUL-padded. `None` if there is no such line, or
/// it is not 4 to 8 digits (a passcode that cannot be typed on the lock
/// screen's keys would lock the screen for good).
///
/// It is not part of [`Theme`]: a theme goes to every program, and to the
/// disk, and a passcode should do neither.
pub fn passcode(text: &str) -> Option<[u8; 8]> {
    let value = text
        .lines()
        .filter_map(|line| uncommented(line).trim().split_once('='))
        .filter(|(key, _)| key.trim() == "passcode")
        .map(|(_, value)| value.trim())
        .last()?;
    if !(4..=8).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut code = [0u8; 8];
    code[..value.len()].copy_from_slice(value.as_bytes());
    Some(code)
}

/// The contents of the file at `path` in a ustar archive, if it is there.
/// Paths are compared without a leading `./` or `/`. Nothing allocated: the
/// kernel reads its theme with this before anything else is running.
pub fn find_in_tar<'a>(archive: &'a [u8], path: &str) -> Option<&'a [u8]> {
    let want = path.trim_start_matches("./").trim_start_matches('/');
    tar_files(archive).find(|(name, _)| *name == want).map(|(_, data)| data)
}

/// Every regular file in a ustar archive: its path, without a leading `./`
/// or `/`, and its contents. Stops at the end, or at anything malformed.
///
/// A long path keeps its start in the prefix field; nothing in `root/`
/// needs it, so such a file is listed by the rest of its name.
pub fn tar_files(archive: &[u8]) -> TarFiles<'_> {
    TarFiles { archive, at: 0 }
}

pub struct TarFiles<'a> {
    archive: &'a [u8],
    at: usize,
}

impl<'a> Iterator for TarFiles<'a> {
    type Item = (&'a str, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        const BLOCK: usize = 512;
        loop {
            let archive = self.archive;
            let header = archive.get(self.at..self.at + BLOCK)?;
            if header.iter().all(|b| *b == 0) {
                return None;
            }
            let field = |range: core::ops::Range<usize>| {
                let bytes = &header[range];
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                core::str::from_utf8(&bytes[..end]).ok()
            };
            let size = field(124..136)
                .and_then(|text| usize::from_str_radix(text.trim(), 8).ok())?;
            let start = self.at + BLOCK;
            let end = start.checked_add(size)?;
            let name = field(0..100)?;
            self.at = start + size.div_ceil(BLOCK) * BLOCK;
            if matches!(header[156], b'0' | 0) {
                let name = name.trim_start_matches("./").trim_start_matches('/');
                return Some((name, archive.get(start..end)?));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_default() {
        assert_eq!(Theme::parse(""), CORRODE);
    }

    #[test]
    fn a_preset_comes_first_wherever_it_is() {
        let theme = Theme::parse("accent = #112233\npreset = breeze\n");
        assert_eq!(theme.accent, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.buttons, Buttons::Right);
    }

    #[test]
    fn comments_and_nonsense_are_skipped() {
        let theme = Theme::parse(
            "# a comment\nglass = clear # trailing\nrefraction = lots\nnot a line\nshadows=off\naccent = #010203 # blue-ish\n",
        );
        assert_eq!(theme.glass, GlassMode::Clear);
        assert_eq!(theme.refraction, CORRODE.refraction);
        assert!(!theme.shadows);
        assert_eq!(theme.accent, Color::rgb(1, 2, 3));
    }

    #[test]
    fn timezones_and_the_clock_parse_and_pack() {
        let theme = Theme::parse("timezone = +05:30\nclock = 12h\n");
        assert_eq!((theme.utc_offset, theme.clock_24h), (330, false));
        assert_eq!(Theme::from_words(theme.to_words()), theme);
        let west = Theme::parse("timezone = -8\n");
        assert_eq!(west.utc_offset, -480);
        assert_eq!(Theme::from_words(west.to_words()), west);
        assert_eq!(offset("+15"), None);
        assert_eq!(offset("5"), None);
        assert_eq!(offset("UTC"), Some(0));
    }

    #[test]
    fn a_kept_theme_comes_back_and_a_damaged_one_does_not() {
        for (name, theme) in PRESETS {
            let record = theme.to_record();
            assert_eq!(Theme::from_record(&record), Some(theme), "{name}");
            // A sector is longer than a record; what follows is not read.
            let mut sector = [0x55u8; 512];
            sector[..RECORD_LEN].copy_from_slice(&record);
            assert_eq!(Theme::from_record(&sector), Some(theme), "{name}");
            for at in [0, 7, 8, 39, 40, 47] {
                let mut damaged = record;
                damaged[at] ^= 1;
                assert_eq!(Theme::from_record(&damaged), None, "{name}, byte {at}");
            }
        }
        assert_eq!(Theme::from_record(&[0u8; 512]), None);
        assert_eq!(Theme::from_record(&[0u8; 10]), None);
    }

    #[test]
    fn a_passcode_is_four_to_eight_digits() {
        assert_eq!(passcode("passcode = 1234\n"), Some(*b"1234\0\0\0\0"));
        assert_eq!(passcode("# lock\npasscode=87654321 # mine\n"), Some(*b"87654321"));
        // The last one wins, as for every other key.
        assert_eq!(passcode("passcode = 1111\npasscode = 2222\n"), Some(*b"2222\0\0\0\0"));
        for bad in ["", "passcode = 123", "passcode = 123456789", "passcode = 12a4", "passcode ="] {
            assert_eq!(passcode(bad), None, "{bad:?}");
        }
        // And the theme does not take it as anything.
        assert_eq!(Theme::parse("passcode = 1234\n"), Theme::default());
    }

    #[test]
    fn magnify_takes_a_word_or_a_percentage() {
        assert_eq!(Theme::parse("magnify = off\n").magnify, 100);
        assert_eq!(Theme::parse("magnify = on\n").magnify, CORRODE.magnify);
        assert_eq!(Theme::parse("magnify = 170\n").magnify, 170);
        // Shrinking is not magnifying, and twice the size is plenty.
        assert_eq!(Theme::parse("magnify = 40\n").magnify, 100);
        assert_eq!(Theme::parse("magnify = 900\n").magnify, 200);
    }

    #[test]
    fn numbers_are_clamped() {
        let theme = Theme::parse("refraction = 9000\ndock_size = 2\n");
        assert_eq!((theme.refraction, theme.dock_size), (120, 24));
    }

    #[test]
    fn every_preset_survives_four_words() {
        for (name, theme) in PRESETS {
            assert_eq!(Theme::from_words(theme.to_words()), theme, "{name}");
        }
        let odd = Theme::parse("tint = #abcdef\ntint_amount = 200\nwallpaper_colors = #010203 #040506 #070809\n");
        assert_eq!(Theme::from_words(odd.to_words()), odd);
    }

    #[test]
    fn colours_are_six_hex_digits() {
        assert_eq!(color("#e2643A"), Some(Color::rgb(0xe2, 0x64, 0x3a)));
        assert_eq!(color("e2643a"), None);
        assert_eq!(color("#e264"), None);
        assert_eq!(color("#gg0000"), None);
    }

    fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in files {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            let size = format!("{:011o}\0", data.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[156] = b'0';
            out.extend_from_slice(&header);
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0; 1024]);
        out
    }

    #[test]
    fn a_file_is_found_in_an_archive() {
        let archive = tar(&[("./README", b"hi"), ("./etc/desktop.conf", b"glass = off\n")]);
        assert_eq!(find_in_tar(&archive, "/etc/desktop.conf"), Some(&b"glass = off\n"[..]));
        assert_eq!(find_in_tar(&archive, "etc/missing"), None);
        assert_eq!(find_in_tar(&archive[..100], "README"), None);
    }

    #[test]
    fn every_file_in_an_archive_is_listed() {
        let archive = tar(&[("./README", b"hi"), ("./etc/motd", b"hello")]);
        let names: Vec<&str> = tar_files(&archive).map(|(name, _)| name).collect();
        assert_eq!(names, ["README", "etc/motd"]);
    }
}
