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
            buttons: 1
        }]
    );
    // Left 2, down 7: both signs set, nine-bit two's complement.
    assert_eq!(
        moved(&[0x38, 0xfe, 0xf9]),
        [MouseEvent {
            dx: -2,
            dy: 7,
            buttons: 0
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
            buttons: 2
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
