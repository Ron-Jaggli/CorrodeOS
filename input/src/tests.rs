use super::*;

fn typed(bytes: &[u8]) -> String {
    let mut keyboard = Keyboard::new();
    bytes
        .iter()
        .filter_map(|byte| keyboard.feed(*byte))
        .filter_map(|event| event.text)
        .collect()
}

fn events(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut keyboard = Keyboard::new();
    bytes
        .iter()
        .filter_map(|byte| keyboard.feed(*byte))
        .collect()
}

#[test]
fn a_key_is_pressed_and_released() {
    // 'a' down, 'a' up.
    let got = events(&[0x1e, 0x9e]);
    assert_eq!(
        got,
        [
            KeyEvent {
                key: Key(0x1e),
                pressed: true,
                text: Some('a')
            },
            KeyEvent {
                key: Key(0x1e),
                pressed: false,
                text: None
            },
        ]
    );
}

#[test]
fn the_rows_type_what_is_printed_on_them() {
    // Scancodes 0x02..=0x0d, 0x10..=0x1b, 0x1e..=0x29, 0x2b..=0x35, pressed.
    let row = |from: u8, to: u8| (from..=to).collect::<Vec<u8>>();
    assert_eq!(typed(&row(0x02, 0x0d)), "1234567890-=");
    assert_eq!(typed(&row(0x10, 0x1b)), "qwertyuiop[]");
    assert_eq!(typed(&row(0x1e, 0x29)), "asdfghjkl;'`");
    assert_eq!(typed(&row(0x2b, 0x35)), "\\zxcvbnm,./");
    assert_eq!(typed(&[0x39, 0x1c, 0x0e, 0x0f]), " \n\x08\t");
}

#[test]
fn shift_is_held_per_side() {
    // Left shift down, h, right shift down, left shift up, i, right up, j.
    assert_eq!(typed(&[0x2a, 0x23, 0x36, 0xaa, 0x17, 0xb6, 0x24]), "HIj");
    assert_eq!(typed(&[0x2a, 0x02, 0x0d, 0x27, 0x28, 0xaa]), "!+:\"");
}

#[test]
fn caps_lock_turns_over_once_per_press_and_only_shifts_letters() {
    // Caps down, repeated while held, up: one turn. Then a, 1, shift+a.
    assert_eq!(
        typed(&[0x3a, 0x3a, 0x3a, 0xba, 0x1e, 0x02, 0x2a, 0x1e, 0xaa]),
        "A1a"
    );
    // Twice is off again.
    assert_eq!(typed(&[0x3a, 0xba, 0x3a, 0xba, 0x1e]), "a");
}

#[test]
fn extended_keys_are_their_own_keys() {
    let got = events(&[0xe0, 0x48, 0xe0, 0xc8, 0xe0, 0x1d, 0xe0, 0x9d]);
    assert_eq!(got[0].key, Key::UP);
    assert!(got[0].pressed && got[0].text.is_none());
    assert_eq!(got[1].key, Key::UP);
    assert!(!got[1].pressed);
    assert_eq!(got[2].key, Key::RIGHT_CTRL);
    // Right Ctrl is not left Ctrl, but it is Ctrl.
    let mut keyboard = Keyboard::new();
    keyboard.feed(0xe0);
    keyboard.feed(0x1d);
    assert_eq!(keyboard.modifiers(), modifier::CTRL);
    keyboard.feed(0xe0);
    keyboard.feed(0x9d);
    assert_eq!(keyboard.modifiers(), 0);
}

#[test]
fn fake_shifts_and_pause_are_swallowed() {
    // Print Screen as sent with Num Lock on: E0 2A E0 37, then E0 B7 E0 AA.
    let got = events(&[0xe0, 0x2a, 0xe0, 0x37, 0xe0, 0xb7, 0xe0, 0xaa]);
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|event| event.key == Key(0x80 | 0x37)));
    // Pause, then 'b': only 'b' comes out, and Ctrl was never held.
    let mut keyboard = Keyboard::new();
    let mut out = Vec::new();
    for byte in [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5, 0x30] {
        out.extend(keyboard.feed(byte));
        assert_eq!(keyboard.modifiers(), 0);
    }
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, Some('b'));
}

#[test]
fn every_byte_sequence_is_survived() {
    // Whatever arrives, the decoder does not panic, and a plain key after
    // it is still understood once any half-finished sequence has run out.
    let mut seed = 0x1234_5678u32;
    for _ in 0..2000 {
        let mut keyboard = Keyboard::new();
        for _ in 0..16 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            keyboard.feed((seed >> 16) as u8);
        }
        // At most six bytes of Pause are pending; feed the release of
        // something harmless until it has drained, then 'z'.
        for _ in 0..6 {
            keyboard.feed(0xb9);
        }
        let event = keyboard.feed(0x2c).unwrap();
        assert_eq!(event.key, Key(0x2c));
        assert!(event.pressed);
    }
}

fn moved(bytes: &[u8]) -> Vec<MouseEvent> {
    let mut mouse = Mouse::new();
    bytes.iter().filter_map(|byte| mouse.feed(*byte)).collect()
}

#[test]
fn a_mouse_packet_is_three_bytes() {
    // Right 5, up 3 (which the mouse reports as +3 and a screen as -3),
    // left button held.
    assert_eq!(
        moved(&[0x09, 5, 3]),
        [MouseEvent {
            dx: 5,
            dy: -3,
            buttons: 1,
            wheel: 0
        }]
    );
    // Left 2, down 7: both signs set, nine-bit two's complement.
    assert_eq!(
        moved(&[0x38, 0xfe, 0xf9]),
        [MouseEvent {
            dx: -2,
            dy: 7,
            buttons: 0,
            wheel: 0
        }]
    );
    // The extremes.
    assert_eq!(moved(&[0x08, 255, 0])[0].dx, 255);
    assert_eq!(moved(&[0x18, 0, 0])[0].dx, -256);
}

#[test]
fn an_overflowed_packet_keeps_its_buttons_and_loses_its_motion() {
    assert_eq!(
        moved(&[0x4a, 200, 100]),
        [MouseEvent {
            dx: 0,
            dy: 0,
            buttons: 2,
            wheel: 0
        }]
    );
}

#[test]
fn a_mouse_stream_joined_in_the_middle_finds_its_packets() {
    // Two stray bytes without bit 3 set, then two real packets.
    let got = moved(&[0x00, 0x07, 0x08, 1, 0, 0x0c, 2, 0]);
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].dx, got[1].dx, got[1].buttons), (1, 2, 4));
}

#[test]
fn a_wheel_mouse_packet_is_four_bytes() {
    let mut mouse = Mouse::with_wheel();
    let got: Vec<MouseEvent> = [0x08, 1, 0, 0x01, 0x08, 0, 0, 0x0f]
        .iter()
        .filter_map(|byte| mouse.feed(*byte))
        .collect();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].dx, got[0].wheel, got[1].wheel), (1, 1, -1));
}

mod usb {
    use crate::usb::{find_hid, BootKeyboard, HidInterface, Layout, PointerReport, ABSOLUTE_MAX};
    use crate::{Key, KeyEvent, Keyboard};

    /// Reports through a boot keyboard and its set-1 bytes through the PS/2
    /// decoder: what the desktop would be told.
    fn through(reports: &[[u8; 8]]) -> Vec<KeyEvent> {
        let mut usb = BootKeyboard::new();
        let mut keyboard = Keyboard::new();
        let mut events = Vec::new();
        for report in reports {
            usb.report(report, &mut |byte| events.extend(keyboard.feed(byte)));
        }
        events
    }

    fn typed(reports: &[[u8; 8]]) -> String {
        through(reports).iter().filter_map(|event| event.text).collect()
    }

    const UP: [u8; 8] = [0; 8];

    #[test]
    fn a_key_goes_down_and_up() {
        let events = through(&[[0, 0, 0x04, 0, 0, 0, 0, 0], UP]);
        assert_eq!(
            events,
            [
                KeyEvent { key: Key(0x1e), pressed: true, text: Some('a') },
                KeyEvent { key: Key(0x1e), pressed: false, text: None },
            ]
        );
    }

    #[test]
    fn shift_in_the_same_report_shifts() {
        // Left Shift and h together, then i with Shift still down, then
        // everything up: "Hi" would be wrong, "HI" right.
        assert_eq!(typed(&[[0x02, 0, 0x0b, 0, 0, 0, 0, 0], [0x02, 0, 0x0c, 0, 0, 0, 0, 0], UP]), "HI");
        // Right Shift is a shift too.
        assert_eq!(typed(&[[0x20, 0, 0x1e, 0, 0, 0, 0, 0], UP]), "!");
    }

    #[test]
    fn keys_held_together_come_and_go_on_their_own() {
        let events = through(&[
            [0, 0, 0x04, 0, 0, 0, 0, 0],
            // b joins a, in another slot.
            [0, 0, 0x05, 0x04, 0, 0, 0, 0],
            // a lets go; b stays.
            [0, 0, 0x05, 0, 0, 0, 0, 0],
            UP,
        ]);
        let said: Vec<(u8, bool)> = events.iter().map(|event| (event.key.0, event.pressed)).collect();
        assert_eq!(said, [(0x1e, true), (0x30, true), (0x1e, false), (0x30, false)]);
    }

    #[test]
    fn extended_keys_and_modifiers_come_through_e0() {
        // Right arrow, then Right Alt, then the left GUI key.
        let events = through(&[[0, 0, 0x4f, 0, 0, 0, 0, 0], UP, [0x40, 0, 0, 0, 0, 0, 0, 0], [0x08, 0, 0, 0, 0, 0, 0, 0], UP]);
        let said: Vec<(u8, bool)> = events.iter().map(|event| (event.key.0, event.pressed)).collect();
        assert_eq!(
            said,
            [(0xcd, true), (0xcd, false), (0xb8, true), (0xdb, true), (0xb8, false), (0xdb, false)]
        );
    }

    #[test]
    fn alt_and_a_letter_is_a_shortcut_as_on_ps2() {
        // Left Alt with q: the desktop's close shortcut. Alt is down before
        // q, so the q press carries it.
        let mut usb = BootKeyboard::new();
        let mut keyboard = Keyboard::new();
        let mut seen = Vec::new();
        for report in [[0x04, 0, 0x14, 0, 0, 0, 0, 0], UP] {
            usb.report(&report, &mut |byte| {
                if let Some(event) = keyboard.feed(byte) {
                    seen.push((event.key.0, event.pressed, keyboard.modifiers()));
                }
            });
        }
        assert_eq!(seen[1], (0x10, true, crate::modifier::ALT));
    }

    #[test]
    fn a_rollover_report_and_a_short_one_change_nothing() {
        let events = through(&[
            [0, 0, 0x04, 0, 0, 0, 0, 0],
            [0, 0, 1, 1, 1, 1, 1, 1],
            [0, 0, 0x04, 0, 0, 0, 0, 0],
            UP,
        ]);
        assert_eq!(events.len(), 2);
        let mut usb = BootKeyboard::new();
        let mut bytes = Vec::new();
        usb.report(&[0, 0, 0x04], &mut |byte| bytes.push(byte));
        assert!(bytes.is_empty());
    }

    #[test]
    fn every_key_on_the_table_round_trips_through_the_decoder() {
        for usage in 0x04..0x66u8 {
            let events = through(&[[0, 0, usage, 0, 0, 0, 0, 0], UP]);
            // Pause (0x48) says nothing; everything else one press and one
            // release of the same key.
            if usage == 0x48 {
                assert!(events.is_empty());
                continue;
            }
            assert_eq!(events.len(), 2, "usage {usage:#x}");
            assert_eq!(events[0].key, events[1].key);
            assert!(events[0].pressed && !events[1].pressed);
        }
    }

    /// QEMU's usb-tablet: three buttons and five bits of padding, X and Y
    /// as absolute 16-bit numbers up to 0x7fff, a relative wheel byte.
    const TABLET: [u8; 74] = [
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
        0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
        0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff,
        0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01,
        0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01,
        0x81, 0x06, 0xc0, 0xc0,
    ];

    #[test]
    fn a_tablet_descriptor_is_read() {
        let layout = Layout::parse(&TABLET).expect("a layout");
        assert!(layout.absolute);
        assert_eq!(layout.report_id, None);
        assert_eq!(layout.buttons, Some((0, 3)));
        assert_eq!((layout.x.offset, layout.x.size, layout.x.max), (8, 16, 0x7fff));
        assert_eq!((layout.y.offset, layout.y.size), (24, 16));
        let wheel = layout.wheel.expect("a wheel");
        assert_eq!((wheel.offset, wheel.size, wheel.signed), (40, 8, true));
        // Left held, right edge, half way down, one notch away.
        assert_eq!(
            layout.decode(&[0x01, 0xff, 0x7f, 0x00, 0x40, 0x01]),
            Some(PointerReport { x: ABSOLUTE_MAX, y: 0x4000, buttons: 1, wheel: -1, absolute: true })
        );
        assert_eq!(layout.decode(&[0x01, 0xff, 0x7f]), None);
    }

    #[test]
    fn a_boot_mouse_moves() {
        let layout = Layout::BOOT_MOUSE;
        assert_eq!(
            layout.decode(&[0x02, 5, 0xfb, 0xff]),
            Some(PointerReport { x: 5, y: -5, buttons: 2, wheel: 1, absolute: false })
        );
        // Three bytes, no wheel.
        assert_eq!(layout.decode(&[0x04, 0x80, 0x7f]).map(|r| (r.x, r.y, r.wheel)), Some((-128, 127, 0)));
    }

    #[test]
    fn numbered_reports_are_matched_by_their_number() {
        // Report 2: three buttons, padding, relative X and Y bytes.
        let descriptor = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
            0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
            0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08,
            0x95, 0x02, 0x81, 0x06, 0xc0,
        ];
        let layout = Layout::parse(&descriptor).expect("a layout");
        assert_eq!(layout.report_id, Some(2));
        assert!(!layout.absolute);
        assert_eq!(layout.decode(&[2, 0x01, 3, 0xfd]).map(|r| (r.x, r.y, r.buttons)), Some((3, -3, 1)));
        assert_eq!(layout.decode(&[1, 0x01, 3, 0xfd]), None);
    }

    #[test]
    fn nonsense_descriptors_are_refused_without_a_panic() {
        assert_eq!(Layout::parse(&[]), None);
        assert_eq!(Layout::parse(&[0x05]), None);
        // Pop with nothing pushed.
        assert_eq!(Layout::parse(&[0xb4]), None);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for len in 0..400 {
            let bytes: Vec<u8> = (0..len % 97)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            if let Some(layout) = Layout::parse(&bytes) {
                let _ = layout.decode(&bytes);
            }
        }
    }

    /// A keyboard's configuration as QEMU's usb-kbd gives it at high speed:
    /// configuration, interface, HID descriptor, endpoint.
    const KEYBOARD_CONFIG: [u8; 34] = [
        0x09, 0x02, 0x22, 0x00, 0x01, 0x01, 0x06, 0xa0, 0x32, //
        0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x07, //
        0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3f, 0x00, //
        0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x07,
    ];

    #[test]
    fn a_keyboard_interface_is_found() {
        assert_eq!(
            find_hid(&KEYBOARD_CONFIG),
            Some(HidInterface {
                configuration: 1,
                interface: 0,
                subclass: 1,
                protocol: 1,
                endpoint: 1,
                max_packet: 8,
                interval: 7,
                report_length: 63,
            })
        );
    }

    #[test]
    fn other_interfaces_and_out_endpoints_are_passed_over() {
        // A mass storage interface with two bulk endpoints, then a HID one
        // with an interrupt OUT endpoint before its IN one.
        let mut config = vec![0x09, 0x02, 0, 0, 0x02, 0x03, 0, 0xa0, 0x32];
        config.extend([0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00]);
        config.extend([0x07, 0x05, 0x81, 0x02, 0x00, 0x02, 0x00]);
        config.extend([0x07, 0x05, 0x02, 0x02, 0x00, 0x02, 0x00]);
        config.extend([0x09, 0x04, 0x01, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00]);
        config.extend([0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x4a, 0x00]);
        config.extend([0x07, 0x05, 0x03, 0x03, 0x40, 0x00, 0x01]);
        config.extend([0x07, 0x05, 0x84, 0x03, 0x40, 0x00, 0x01]);
        let hid = find_hid(&config).expect("the HID interface");
        assert_eq!((hid.configuration, hid.interface, hid.endpoint), (3, 1, 4));
        assert_eq!((hid.max_packet, hid.report_length), (64, 74));
    }

    #[test]
    fn broken_configurations_are_refused_without_a_panic() {
        assert_eq!(find_hid(&[]), None);
        assert_eq!(find_hid(&KEYBOARD_CONFIG[..20]), None);
        // A descriptor of length 0 would never move on; one of 40 runs off.
        let mut zero = KEYBOARD_CONFIG;
        zero[9] = 0;
        assert_eq!(find_hid(&zero), None);
        let mut long = KEYBOARD_CONFIG;
        long[27] = 40;
        assert_eq!(find_hid(&long), None);
        // Endpoint 0 is the control endpoint, never an interrupt one.
        let mut zeroth = KEYBOARD_CONFIG;
        zeroth[29] = 0x80;
        assert_eq!(find_hid(&zeroth), None);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let mut config = KEYBOARD_CONFIG;
            for _ in 0..3 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                config[(state % 34) as usize] = (state >> 8) as u8;
            }
            let _ = find_hid(&config);
        }
    }
}
