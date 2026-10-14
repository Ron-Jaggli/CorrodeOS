//! Commands as bytes.
//!
//! A [`Command`] is plain data, which is what lets a command list leave the
//! process that recorded it. But plain data is not yet a format: two
//! programs built separately can each be sure a `Command` has no pointers in
//! it and still disagree about where its fields are, since nothing promises
//! a Rust enum's layout. So this is the format. Every command is one
//! [`RECORD`]-byte record, little-endian, with its kind in the first byte,
//! and decoding is written out field by field.
//!
//! Decoding is the side that matters. The bytes come from another protection
//! domain, so an unknown kind is `None`, not a guess, and every field is
//! taken as whatever number it is -- a rectangle anywhere at all is a
//! rectangle the renderer clips, which it does for every command already.

use crate::{Color, Command, Rect};

/// Bytes per command.
pub const RECORD: usize = 32;
/// Bytes per rectangle, on its own.
pub const RECT: usize = 16;

const CLEAR: u8 = 1;
const FILL_RECT: u8 = 2;
const BLEND_RECT: u8 = 3;
const MASK: u8 = 4;
const IMAGE: u8 = 5;
const SCISSOR: u8 = 6;

/// Write a rectangle as four little-endian words: x, y, width, height.
pub fn put_rect(rect: &Rect, out: &mut [u8; RECT]) {
    out[0..4].copy_from_slice(&rect.x.to_le_bytes());
    out[4..8].copy_from_slice(&rect.y.to_le_bytes());
    out[8..12].copy_from_slice(&rect.width.to_le_bytes());
    out[12..16].copy_from_slice(&rect.height.to_le_bytes());
}

/// Read one back. Any sixteen bytes are some rectangle.
pub fn get_rect(bytes: &[u8; RECT]) -> Rect {
    let word = |at: usize| [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
    Rect::new(
        i32::from_le_bytes(word(0)),
        i32::from_le_bytes(word(4)),
        u32::from_le_bytes(word(8)),
        u32::from_le_bytes(word(12)),
    )
}

/// Write a command.
///
/// ```text
///   0       kind
///   1..4    colour, r g b             (or, for an image, nothing)
///   4..8    surface number            (image only)
///   7       alpha                     (blend, mask, image)
///   8..24   rectangle                 (the source block, for mask and image)
///   24..32  x, y of where it goes     (mask and image)
/// ```
pub fn encode(command: &Command, out: &mut [u8; RECORD]) {
    *out = [0; RECORD];
    let color = |out: &mut [u8; RECORD], color: &Color| {
        out[1] = color.r;
        out[2] = color.g;
        out[3] = color.b;
    };
    let rect = |out: &mut [u8; RECORD], rect: &Rect| {
        let mut bytes = [0; RECT];
        put_rect(rect, &mut bytes);
        out[8..24].copy_from_slice(&bytes);
    };
    let position = |out: &mut [u8; RECORD], x: i32, y: i32| {
        out[24..28].copy_from_slice(&x.to_le_bytes());
        out[28..32].copy_from_slice(&y.to_le_bytes());
    };

    match command {
        Command::Clear { color: c } => {
            out[0] = CLEAR;
            color(out, c);
        }
        Command::FillRect { rect: r, color: c } => {
            out[0] = FILL_RECT;
            color(out, c);
            rect(out, r);
        }
        Command::BlendRect {
            rect: r,
            color: c,
            alpha,
        } => {
            out[0] = BLEND_RECT;
            color(out, c);
            out[7] = *alpha;
            rect(out, r);
        }
        Command::Mask {
            x,
            y,
            src,
            color: c,
            alpha,
        } => {
            out[0] = MASK;
            color(out, c);
            out[7] = *alpha;
            rect(out, src);
            position(out, *x, *y);
        }
        Command::Image {
            x,
            y,
            src,
            surface,
            alpha,
        } => {
            out[0] = IMAGE;
            out[4..7].copy_from_slice(&surface.to_le_bytes()[..3]);
            out[7] = *alpha;
            rect(out, src);
            position(out, *x, *y);
        }
        Command::Scissor { rect: r } => {
            out[0] = SCISSOR;
            rect(out, r);
        }
    }
}

/// Read a command, or `None` if the record is not one.
pub fn decode(bytes: &[u8; RECORD]) -> Option<Command> {
    let color = Color::rgb(bytes[1], bytes[2], bytes[3]);
    let alpha = bytes[7];
    let mut rect_bytes = [0; RECT];
    rect_bytes.copy_from_slice(&bytes[8..24]);
    let rect = get_rect(&rect_bytes);
    let x = i32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let y = i32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);

    Some(match bytes[0] {
        CLEAR => Command::Clear { color },
        FILL_RECT => Command::FillRect { rect, color },
        BLEND_RECT => Command::BlendRect { rect, color, alpha },
        MASK => Command::Mask {
            x,
            y,
            src: rect,
            color,
            alpha,
        },
        IMAGE => Command::Image {
            x,
            y,
            src: rect,
            // Three bytes: more surfaces than any renderer has, and the
            // fourth is where alpha lives.
            surface: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], 0]),
            alpha,
        },
        SCISSOR => Command::Scissor { rect },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(command: Command) -> Option<Command> {
        let mut bytes = [0; RECORD];
        encode(&command, &mut bytes);
        decode(&bytes)
    }

    fn same(a: &Command, b: &Command) -> bool {
        // Command has no PartialEq; its Debug form names every field.
        format!("{a:?}") == format!("{b:?}")
    }

    #[test]
    fn every_kind_of_command_survives_the_trip() {
        let rect = Rect::new(-5, 7, 300, 2);
        let color = Color::rgb(0xb7, 0x41, 0x0e);
        let commands = [
            Command::Clear { color },
            Command::FillRect { rect, color },
            Command::BlendRect {
                rect,
                color,
                alpha: 150,
            },
            Command::Mask {
                x: -1,
                y: 2_000_000,
                src: Rect::new(0, 96, 512, 40),
                color,
                alpha: 255,
            },
            Command::Image {
                x: 976,
                y: 48,
                src: Rect::new(0, 0, 256, 192),
                surface: 7,
                alpha: 1,
            },
            Command::Scissor { rect },
        ];
        for command in commands {
            let back = round_trip(command).expect("decodes");
            assert!(same(&command, &back), "{command:?} came back as {back:?}");
        }
    }

    #[test]
    fn extreme_rectangles_are_still_rectangles() {
        let rect = Rect::new(i32::MIN, i32::MAX, u32::MAX, 0);
        let mut bytes = [0; RECT];
        put_rect(&rect, &mut bytes);
        assert_eq!(get_rect(&bytes), rect);
    }

    #[test]
    fn a_record_that_is_not_a_command_is_refused() {
        for kind in [0u8, 7, 0x80, 0xff] {
            let mut bytes = [0x5a; RECORD];
            bytes[0] = kind;
            assert!(decode(&bytes).is_none());
        }
    }

    #[test]
    fn the_layout_is_the_documented_one() {
        let mut bytes = [0; RECORD];
        encode(
            &Command::BlendRect {
                rect: Rect::new(1, 2, 3, 4),
                color: Color::rgb(10, 20, 30),
                alpha: 99,
            },
            &mut bytes,
        );
        assert_eq!(
            bytes,
            [
                BLEND_RECT, 10, 20, 30, 0, 0, 0, 99, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }
}
