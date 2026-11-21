//! What a keyboard and a mouse say, turned into what they mean.
//!
//! A PS/2 controller hands its driver one byte at a time and says nothing
//! about where one key press ends and the next begins. A key is one byte, or
//! two after an `0xE0`, or six for Pause; a mouse movement is three bytes
//! that have to be kept in step. None of that needs a machine, so it lives
//! here, as two state machines fed a byte at a time, and the tests run on the
//! build host.
//!
//! Neither allocates and neither can fail. A byte that makes no sense is
//! dropped, and the decoder finds its footing again on the bytes after it:
//! input is the one thing the user is guaranteed to keep producing, so a
//! decoder that can get stuck will.

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
mod tests;

pub mod usb;

/// A key, by where it is rather than what it says.
///
/// The number is the key's scancode in set 1, which is what a controller
/// with translation on delivers whatever the keyboard speaks, and keys
/// reached through `0xE0` have the top bit set. That gives every key one
/// byte and leaves "what character is this" to [`Keyboard::char_of`], where
/// layouts belong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key(pub u8);

impl Key {
    pub const ESCAPE: Key = Key(0x01);
    pub const BACKSPACE: Key = Key(0x0e);
    pub const TAB: Key = Key(0x0f);
    pub const ENTER: Key = Key(0x1c);
    pub const LEFT_CTRL: Key = Key(0x1d);
    pub const LEFT_SHIFT: Key = Key(0x2a);
    pub const RIGHT_SHIFT: Key = Key(0x36);
    pub const LEFT_ALT: Key = Key(0x38);
    pub const SPACE: Key = Key(0x39);
    pub const CAPS_LOCK: Key = Key(0x3a);
    pub const RIGHT_CTRL: Key = Key(0x80 | 0x1d);
    pub const RIGHT_ALT: Key = Key(0x80 | 0x38);
    pub const HOME: Key = Key(0x80 | 0x47);
    pub const UP: Key = Key(0x80 | 0x48);
    pub const PAGE_UP: Key = Key(0x80 | 0x49);
    pub const LEFT: Key = Key(0x80 | 0x4b);
    pub const RIGHT: Key = Key(0x80 | 0x4d);
    pub const END: Key = Key(0x80 | 0x4f);
    pub const DOWN: Key = Key(0x80 | 0x50);
    pub const PAGE_DOWN: Key = Key(0x80 | 0x51);
    pub const DELETE: Key = Key(0x80 | 0x53);
}

/// A key going down or coming up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
    /// What it types, given the modifiers at the time, if anything. Only on
    /// a press: letting go of a key types nothing.
    pub text: Option<char>,
}

/// Modifier keys, as bits.
pub mod modifier {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const CAPS: u8 = 1 << 3;
}

/// Where the keyboard decoder is in a sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Nothing,
    /// After `0xE0`.
    Extended,
    /// Inside the Pause key's `E1 1D 45 E1 9D C5`, with this many left.
    Pause(u8),
}

/// Scancode set 1, a byte at a time.
#[derive(Clone, Copy, Debug)]
pub struct Keyboard {
    pending: Pending,
    /// Shift held, per side, so letting go of one with the other still down
    /// does not undo it.
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    caps: bool,
    caps_held: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyboard {
    pub const fn new() -> Self {
        Self {
            pending: Pending::Nothing,
            shift: [false; 2],
            ctrl: [false; 2],
            alt: [false; 2],
            caps: false,
            caps_held: false,
        }
    }

    /// The modifiers in force now, as [`modifier`] bits.
    pub fn modifiers(&self) -> u8 {
        let mut bits = 0;
        if self.shift[0] || self.shift[1] {
            bits |= modifier::SHIFT;
        }
        if self.ctrl[0] || self.ctrl[1] {
            bits |= modifier::CTRL;
        }
        if self.alt[0] || self.alt[1] {
            bits |= modifier::ALT;
        }
        if self.caps {
            bits |= modifier::CAPS;
        }
        bits
    }

    /// Take one byte from the controller. A whole key event comes out when
    /// the byte finishes one.
    pub fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        let extended = match self.pending {
            Pending::Pause(left) => {
                // Pause has no release and means nothing here. Swallow it
                // whole rather than read its bytes as other keys: the `1D`
                // in the middle of it is Ctrl.
                self.pending = if left > 1 {
                    Pending::Pause(left - 1)
                } else {
                    Pending::Nothing
                };
                return None;
            }
            Pending::Extended => {
                self.pending = Pending::Nothing;
                true
            }
            Pending::Nothing => match byte {
                0xe0 => {
                    self.pending = Pending::Extended;
                    return None;
                }
                0xe1 => {
                    self.pending = Pending::Pause(5);
                    return None;
                }
                _ => false,
            },
        };

        let code = byte & 0x7f;
        let pressed = byte & 0x80 == 0;
        // A keyboard pretends Shift was pressed or released around some
        // extended keys, so that software from 1984 sees the code it
        // expects. Those are not keys.
        if code == 0 || (extended && (code == 0x2a || code == 0x36)) {
            return None;
        }
        let key = Key(code | if extended { 0x80 } else { 0 });

        match key {
            Key::LEFT_SHIFT => self.shift[0] = pressed,
            Key::RIGHT_SHIFT => self.shift[1] = pressed,
            Key::LEFT_CTRL => self.ctrl[0] = pressed,
            Key::RIGHT_CTRL => self.ctrl[1] = pressed,
            Key::LEFT_ALT => self.alt[0] = pressed,
            Key::RIGHT_ALT => self.alt[1] = pressed,
            Key::CAPS_LOCK => {
                // A held key repeats its press. Caps Lock turns over once per
                // time it goes down, not once per repeat.
                if pressed && !self.caps_held {
                    self.caps = !self.caps;
                }
                self.caps_held = pressed;
            }
            _ => {}
        }

        Some(KeyEvent {
            key,
            pressed,
            text: if pressed {
                Self::char_of(key, self.modifiers())
            } else {
                None
            },
        })
    }

    /// What `key` types with the modifier bits `modifiers`, on a US layout.
    pub fn char_of(key: Key, modifiers: u8) -> Option<char> {
        let index = key.0 as usize;
        if key.0 & 0x80 != 0 || index >= PLAIN.len() {
            return None;
        }
        let plain = PLAIN[index];
        if plain == 0 {
            return None;
        }
        let shift = modifiers & modifier::SHIFT != 0;
        // Caps Lock is a shift for letters and nothing else.
        let caps = modifiers & modifier::CAPS != 0 && plain.is_ascii_lowercase();
        let byte = if shift != caps { SHIFTED[index] } else { plain };
        Some(byte as char)
    }
}

/// What each key types unshifted, by set 1 scancode. Zero for keys that type
/// nothing.
const PLAIN: [u8; 0x3a] =
    *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";

/// And shifted.
const SHIFTED: [u8; 0x3a] =
    *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

/// A mouse moving or a button changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    /// Right is positive.
    pub dx: i16,
    /// Down is positive, as on a screen. The mouse says the opposite.
    pub dy: i16,
    /// Bit 0 left, bit 1 right, bit 2 middle: held, not clicked.
    pub buttons: u8,
    /// Wheel notches: toward the person (scrolling down) is positive.
    pub wheel: i8,
}

/// The standard three-byte PS/2 mouse packet, a byte at a time -- or the
/// IntelliMouse's four, whose last byte is the wheel.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mouse {
    bytes: [u8; 4],
    have: usize,
    wheel: bool,
}

impl Mouse {
    pub const fn new() -> Self {
        Self {
            bytes: [0; 4],
            have: 0,
            wheel: false,
        }
    }

    /// A mouse that has been switched into IntelliMouse mode: four bytes a
    /// packet.
    pub const fn with_wheel() -> Self {
        Self {
            wheel: true,
            ..Self::new()
        }
    }

    /// Take one byte from the mouse. An event comes out on every third
    /// byte that belongs with the two before it.
    pub fn feed(&mut self, byte: u8) -> Option<MouseEvent> {
        // The first byte of a packet always has bit 3 set. A byte without it
        // cannot start one, so it is dropped: that is how a stream that
        // started half way through a packet, or lost a byte, finds packets
        // again.
        if self.have == 0 && byte & 0x08 == 0 {
            return None;
        }
        self.bytes[self.have] = byte;
        self.have += 1;
        if self.have < if self.wheel { 4 } else { 3 } {
            return None;
        }
        self.have = 0;

        let [flags, x, y, z] = self.bytes;
        // Four bits, two's complement; the mouse counts away from the person
        // as negative already.
        let wheel = if self.wheel { ((z << 4) as i8) >> 4 } else { 0 };
        // A movement too large for nine bits is reported as overflowed, and
        // the number that came with it is meaningless. Keep the buttons.
        let overflow = flags & 0xc0 != 0;
        let signed = |value: u8, negative: bool| {
            if overflow {
                0
            } else if negative {
                value as i16 - 256
            } else {
                value as i16
            }
        };
        Some(MouseEvent {
            dx: signed(x, flags & 0x10 != 0),
            dy: -signed(y, flags & 0x20 != 0),
            buttons: flags & 0x07,
            wheel,
        })
    }
}
