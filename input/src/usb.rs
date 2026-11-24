//! What USB keyboards and pointers say, turned into what the PS/2 path
//! already understands.
//!
//! A USB keyboard in the boot protocol sends an eight-byte report whenever
//! anything changes: a byte of modifier bits, a byte reserved, and the usage
//! codes of up to six keys held, in no order. Nothing says which key went
//! down or up; that is the difference between two reports. [`BootKeyboard`]
//! works it out and says it in scancode set 1, so the same [`Keyboard`]
//! decoder -- modifiers, Caps Lock, what a key types -- serves both buses.
//!
//! A pointer's reports are laid out by its report descriptor, a small
//! bytecode of global and local state and `Input` items. [`Layout`] reads
//! the part of it a mouse or a tablet needs (buttons, X, Y, wheel) and
//! [`Layout::decode`] pulls those fields out of a report. A boot-protocol
//! mouse has a fixed layout, [`Layout::BOOT_MOUSE`].
//!
//! [`Keyboard`]: crate::Keyboard

/// Set-1 scancode for each USB keyboard usage from 0x04 (`a`) on: the code,
/// with `EXT` added for keys reached through `0xE0`. Zero for none.
const EXT: u16 = 0x100;
const SET1: [u16; 0x66 - 4] = [
    // a to z.
    0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, //
    0x31, 0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c, //
    // 1 to 0.
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, //
    // Enter, Escape, Backspace, Tab, Space, - = [ ] \, non-US #, ; ' ` , . /
    0x1c, 0x01, 0x0e, 0x0f, 0x39, 0x0c, 0x0d, 0x1a, 0x1b, 0x2b, 0x2b, 0x27, 0x28, //
    0x29, 0x33, 0x34, 0x35, //
    // Caps Lock, F1 to F12.
    0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58, //
    // Print Screen, Scroll Lock, Pause (its own six bytes; left out).
    EXT | 0x37, 0x46, 0, //
    // Insert, Home, Page Up, Delete, End, Page Down.
    EXT | 0x52, EXT | 0x47, EXT | 0x49, EXT | 0x53, EXT | 0x4f, EXT | 0x51, //
    // Right, Left, Down, Up.
    EXT | 0x4d, EXT | 0x4b, EXT | 0x50, EXT | 0x48, //
    // Num Lock, keypad / * - + Enter 1 2 3 4 5 6 7 8 9 0 .
    0x45, EXT | 0x35, 0x37, 0x4a, 0x4e, EXT | 0x1c, 0x4f, 0x50, 0x51, 0x4b, 0x4c, //
    0x4d, 0x47, 0x48, 0x49, 0x52, 0x53, //
    // The key between left Shift and Z on ISO boards, Menu.
    0x56, EXT | 0x5d,
];

/// The modifier byte's bits, in order: left Ctrl, Shift, Alt, GUI, then the
/// right ones.
const MODIFIERS: [u16; 8] = [
    0x1d,
    0x2a,
    0x38,
    EXT | 0x5b,
    EXT | 0x1d,
    0x36,
    EXT | 0x38,
    EXT | 0x5c,
];

/// The set-1 code for a keyboard usage, `EXT` added if it is extended.
fn set1(usage: u8) -> Option<u16> {
    let code = *SET1.get((usage as usize).checked_sub(4)?)?;
    (code != 0).then_some(code)
}

/// Say `code` going down or up in set-1 bytes.
fn say(code: u16, pressed: bool, out: &mut impl FnMut(u8)) {
    if code & EXT != 0 {
        out(0xe0);
    }
    out(code as u8 | if pressed { 0 } else { 0x80 });
}

/// A boot-protocol keyboard, a report at a time.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootKeyboard {
    modifiers: u8,
    held: [u8; 6],
}

impl BootKeyboard {
    pub const fn new() -> Self {
        Self {
            modifiers: 0,
            held: [0; 6],
        }
    }

    /// Take one report and say, in set-1 bytes, what changed since the last:
    /// modifiers going down first, so a key pressed with Shift in the same
    /// report types its shifted character; then keys let go, keys pressed,
    /// and modifiers let go last. A report shorter than eight bytes, or one
    /// saying too many keys are held to tell which (every slot 0x01), is
    /// ignored: the next good one is compared with the last good one.
    pub fn report(&mut self, report: &[u8], out: &mut impl FnMut(u8)) {
        let Some(report) = report.get(..8) else {
            return;
        };
        let keys = &report[2..8];
        if keys.iter().all(|&usage| usage == 0x01) {
            return;
        }
        let modifiers = report[0];
        let changed = modifiers ^ self.modifiers;
        for (bit, code) in MODIFIERS.iter().enumerate() {
            if changed & modifiers & (1 << bit) != 0 {
                say(*code, true, out);
            }
        }
        for &usage in &self.held {
            if usage != 0 && !keys.contains(&usage) {
                if let Some(code) = set1(usage) {
                    say(code, false, out);
                }
            }
        }
        for &usage in keys {
            if usage != 0 && !self.held.contains(&usage) {
                if let Some(code) = set1(usage) {
                    say(code, true, out);
                }
            }
        }
        for (bit, code) in MODIFIERS.iter().enumerate() {
            if changed & !modifiers & (1 << bit) != 0 {
                say(*code, false, out);
            }
        }
        self.modifiers = modifiers;
        self.held.copy_from_slice(keys);
    }
}

/// Where one value sits in a report, in bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub offset: u32,
    pub size: u32,
    /// Read as two's complement.
    pub signed: bool,
    /// The largest value the device reports, for scaling an absolute axis.
    pub max: i32,
}

impl Field {
    /// The field's value in `report`, `None` if the report is too short.
    pub fn read(&self, report: &[u8]) -> Option<i32> {
        if self.size == 0 || self.size > 32 {
            return None;
        }
        let end = self.offset + self.size;
        if end.div_ceil(8) as usize > report.len() {
            return None;
        }
        let mut value: u64 = 0;
        for bit in 0..self.size {
            let at = self.offset + bit;
            if report[(at / 8) as usize] & (1 << (at % 8)) != 0 {
                value |= 1 << bit;
            }
        }
        if self.signed && self.size < 64 && value & (1 << (self.size - 1)) != 0 {
            value |= u64::MAX << self.size;
        }
        Some(value as i64 as i32)
    }
}

/// Where a pointer's buttons, axes and wheel are in its reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The report ID byte that starts the reports this describes, if the
    /// device numbers its reports. Offsets count from after it.
    pub report_id: Option<u8>,
    /// The first button's bit, and how many buttons.
    pub buttons: Option<(u32, u32)>,
    pub x: Field,
    pub y: Field,
    pub wheel: Option<Field>,
    /// X and Y are positions, not movements.
    pub absolute: bool,
}

/// One pointer report, read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerReport {
    /// Movements, or positions scaled to 0..=[`ABSOLUTE_MAX`].
    pub x: i32,
    pub y: i32,
    /// Bit 0 left, bit 1 right, bit 2 middle.
    pub buttons: u8,
    /// Notches; toward the person (scrolling down) positive, as for PS/2.
    pub wheel: i32,
    pub absolute: bool,
}

/// What an absolute position is scaled to: the far edge.
pub const ABSOLUTE_MAX: i32 = 0x7fff;

/// Usage pages and usages read from a report descriptor.
const GENERIC_DESKTOP: u32 = 0x01;
const BUTTON: u32 = 0x09;
const X: u32 = 0x30;
const Y: u32 = 0x31;
const WHEEL: u32 = 0x38;

impl Layout {
    /// A boot-protocol mouse: a byte of buttons, then X and Y movements and
    /// (if the report has a fourth byte) the wheel, each a signed byte.
    pub const BOOT_MOUSE: Layout = Layout {
        report_id: None,
        buttons: Some((0, 3)),
        x: Field { offset: 8, size: 8, signed: true, max: 127 },
        y: Field { offset: 16, size: 8, signed: true, max: 127 },
        wheel: Some(Field { offset: 24, size: 8, signed: true, max: 127 }),
        absolute: false,
    };

    /// Read a report descriptor for the first report with an X and a Y in
    /// it. `None` if there is none, or the descriptor makes no sense.
    pub fn parse(descriptor: &[u8]) -> Option<Layout> {
        // Global state, and the stack Push and Pop keep it on.
        #[derive(Clone, Copy, Default)]
        struct Global {
            page: u32,
            minimum: i32,
            maximum: i32,
            size: u32,
            count: u32,
            report_id: Option<u8>,
        }
        let mut global = Global::default();
        let mut stack = [Global::default(); 4];
        let mut depth = 0;
        // Local state, cleared after every main item.
        let mut usages = [0u32; 16];
        let mut used = 0;
        let mut range: Option<(u32, u32)> = None;
        // Bits so far in each report, by ID (no ID counts as 0).
        let mut offsets = [0u32; 256];

        let mut found = Layout {
            report_id: None,
            buttons: None,
            x: Field { offset: 0, size: 0, signed: false, max: 0 },
            y: Field { offset: 0, size: 0, signed: false, max: 0 },
            wheel: None,
            absolute: false,
        };
        let mut chosen: Option<Option<u8>> = None;

        let mut at = 0;
        while at < descriptor.len() {
            let prefix = descriptor[at];
            if prefix == 0xfe {
                // A long item: never needed here, skipped whole.
                let len = *descriptor.get(at + 1)? as usize;
                at += 3 + len;
                continue;
            }
            let len = [0, 1, 2, 4][(prefix & 3) as usize];
            let data = descriptor.get(at + 1..at + 1 + len)?;
            at += 1 + len;
            let unsigned = data.iter().rev().fold(0u32, |acc, byte| acc << 8 | *byte as u32);
            let signed = match len {
                1 => data[0] as i8 as i32,
                2 => i16::from_le_bytes([data[0], data[1]]) as i32,
                4 => unsigned as i32,
                _ => 0,
            };
            let tag = prefix & 0xfc;
            match tag {
                // Main items: Input, Output, Feature, Collection, End.
                0x80 => {
                    let id = global.report_id;
                    let offset = &mut offsets[id.unwrap_or(0) as usize];
                    let constant = unsigned & 1 != 0;
                    let variable = unsigned & 2 != 0;
                    let relative = unsigned & 4 != 0;
                    let size = global.size;
                    let mine = chosen.is_none_or(|chosen| chosen == id);
                    if !constant && variable && mine {
                        for index in 0..global.count {
                            let usage = match range {
                                Some((low, high)) => (low + index).min(high),
                                None if used == 0 => 0,
                                None => usages[(index as usize).min(used - 1)],
                            };
                            let page = if usage >> 16 != 0 { usage >> 16 } else { global.page };
                            let usage = usage & 0xffff;
                            let field = Field {
                                offset: *offset + index * size,
                                size,
                                signed: global.minimum < 0,
                                max: global.maximum,
                            };
                            match (page, usage) {
                                (BUTTON, 1) => {
                                    found.buttons = Some((field.offset, global.count.min(3)));
                                }
                                (GENERIC_DESKTOP, X) => {
                                    found.x = field;
                                    found.absolute = !relative;
                                    chosen = Some(id);
                                }
                                (GENERIC_DESKTOP, Y) => found.y = field,
                                (GENERIC_DESKTOP, WHEEL) => found.wheel = Some(field),
                                _ => {}
                            }
                        }
                    }
                    *offset = offset.checked_add(size.checked_mul(global.count)?)?;
                }
                0x90 | 0xb0 => {}
                0xa0 | 0xc0 => {}
                // Global items.
                0x04 => global.page = unsigned,
                0x14 => global.minimum = signed,
                0x24 => {
                    // A maximum is read unsigned when the minimum is not
                    // negative: 0xff with a minimum of 0 is 255.
                    global.maximum = if global.minimum >= 0 { unsigned as i32 } else { signed }
                }
                0x74 => global.size = unsigned,
                0x84 => global.report_id = Some(unsigned as u8),
                0x94 => global.count = unsigned,
                0xa4 => {
                    *stack.get_mut(depth)? = global;
                    depth += 1;
                }
                0xb4 => {
                    depth = depth.checked_sub(1)?;
                    global = stack[depth];
                }
                // Local items.
                0x08 => {
                    if used < usages.len() {
                        usages[used] = unsigned;
                        used += 1;
                    }
                }
                0x18 => range = Some((unsigned, range.map_or(unsigned, |(_, high)| high))),
                0x28 => range = Some((range.map_or(0, |(low, _)| low), unsigned)),
                _ => {}
            }
            if tag & 0x0c == 0 {
                // After a main item, the local state starts again.
                used = 0;
                range = None;
            }
        }
        let id = chosen?;
        if found.y.size == 0 {
            return None;
        }
        found.report_id = id;
        Some(found)
    }

    /// Read one report. `None` if it is not one this layout describes.
    pub fn decode(&self, report: &[u8]) -> Option<PointerReport> {
        let body = match self.report_id {
            Some(id) => report.strip_prefix(&[id])?,
            None => report,
        };
        let buttons = match self.buttons {
            Some((offset, count)) => {
                Field { offset, size: count, signed: false, max: 0 }.read(body)? as u8
            }
            None => 0,
        };
        let scale = |field: &Field, value: i32| {
            if self.absolute && field.max > 0 {
                (value.clamp(0, field.max) as i64 * ABSOLUTE_MAX as i64 / field.max as i64) as i32
            } else {
                value
            }
        };
        let x = self.x.read(body)?;
        let y = self.y.read(body)?;
        Some(PointerReport {
            x: scale(&self.x, x),
            y: scale(&self.y, y),
            buttons,
            // USB counts a notch away from the person as positive.
            wheel: self.wheel.and_then(|wheel| wheel.read(body)).map_or(0, |notches| -notches),
            absolute: self.absolute,
        })
    }
}

/// A HID interface in a configuration descriptor, with its interrupt IN
/// endpoint: what a driver needs to start reading reports from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidInterface {
    /// The configuration's value, for SET_CONFIGURATION.
    pub configuration: u8,
    pub interface: u8,
    /// [`BOOT`] if the interface can speak the boot protocol.
    pub subclass: u8,
    /// [`KEYBOARD`] or [`MOUSE`] for a boot interface; anything for others.
    pub protocol: u8,
    /// The endpoint's number, 1 to 15.
    pub endpoint: u8,
    pub max_packet: u16,
    /// How often to ask, as the endpoint descriptor says it: milliseconds
    /// at low and full speed, a power of two of 125 us at high speed.
    pub interval: u8,
    /// How long the report descriptor is, from the HID descriptor.
    pub report_length: u16,
}

pub const BOOT: u8 = 1;
pub const KEYBOARD: u8 = 1;
pub const MOUSE: u8 = 2;

/// Descriptor types (USB 2.0 9.4, HID 1.11 7.1).
const CONFIGURATION: u8 = 2;
const INTERFACE: u8 = 4;
const ENDPOINT: u8 = 5;
const HID: u8 = 0x21;
const HID_CLASS: u8 = 3;

/// The first HID interface in a whole configuration descriptor that has an
/// interrupt IN endpoint. `None` if there is none, or the descriptors do not
/// fit together: a length that runs off the end, or is too short to be one.
pub fn find_hid(config: &[u8]) -> Option<HidInterface> {
    if config.get(1) != Some(&CONFIGURATION) {
        return None;
    }
    let configuration = *config.get(5)?;
    let mut current: Option<HidInterface> = None;
    let mut at = 0;
    while at < config.len() {
        let len = config[at] as usize;
        let descriptor = config.get(at..at + len).filter(|_| len >= 2)?;
        match descriptor[1] {
            INTERFACE if len >= 9 => {
                current = (descriptor[5] == HID_CLASS).then_some(HidInterface {
                    configuration,
                    interface: descriptor[2],
                    subclass: descriptor[6],
                    protocol: descriptor[7],
                    endpoint: 0,
                    max_packet: 0,
                    interval: 0,
                    report_length: 0,
                });
            }
            HID if len >= 9 => {
                if let Some(hid) = current.as_mut() {
                    hid.report_length = u16::from_le_bytes([descriptor[7], descriptor[8]]);
                }
            }
            ENDPOINT if len >= 7 => {
                let address = descriptor[2];
                let interrupt = descriptor[3] & 3 == 3;
                if let Some(mut hid) = current.filter(|_| address & 0x80 != 0 && interrupt) {
                    hid.endpoint = address & 0x0f;
                    hid.max_packet = u16::from_le_bytes([descriptor[4], descriptor[5]]) & 0x7ff;
                    hid.interval = descriptor[6];
                    if hid.endpoint != 0 && hid.max_packet != 0 {
                        return Some(hid);
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    None
}
