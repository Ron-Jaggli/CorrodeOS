//! A driver for the PS/2 controller: the keyboard and the mouse.
//!
//! An ordinary ring 3 program holding five capabilities, listed in
//! `corrode_abi::ps2`: the controller's data port and its command port, one
//! interrupt line that both devices raise, an endpoint to the compositor and
//! one to the log. It can touch no other port and hear no other device.
//!
//! It sets the controller up, then sleeps until either device has said
//! something, reads everything waiting, and passes on what it meant. The
//! decoding is `corrode-input`, which is tested on the build host; this is
//! the part that talks to hardware.
//!
//! Register and command numbers are the i8042's, as every PC since the AT
//! has emulated them.

#![no_std]
#![no_main]

use corrode_input::{Keyboard, Mouse};
use corrode_rt::abi::ps2::{
    COMMAND, DATA, EVENTS, IRQ, LABEL_KEY, LABEL_POINTER, LABEL_READY, LOG,
};
use corrode_rt::device::{Interrupt, Ports};
use corrode_rt::{println, send, Message};

corrode_rt::entry!(main);

/// Status register bits.
const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;
/// The byte waiting came from the mouse, not the keyboard.
const FROM_MOUSE: u8 = 1 << 5;

/// Controller commands.
const READ_CONFIG: u8 = 0x20;
const WRITE_CONFIG: u8 = 0x60;
const DISABLE_MOUSE: u8 = 0xa7;
const ENABLE_MOUSE: u8 = 0xa8;
const DISABLE_KEYBOARD: u8 = 0xad;
const ENABLE_KEYBOARD: u8 = 0xae;
/// The next byte written to the data port goes to the mouse.
const TO_MOUSE: u8 = 0xd4;

/// Configuration byte bits.
const CONFIG_KEYBOARD_IRQ: u8 = 1 << 0;
const CONFIG_MOUSE_IRQ: u8 = 1 << 1;
const CONFIG_KEYBOARD_OFF: u8 = 1 << 4;
const CONFIG_MOUSE_OFF: u8 = 1 << 5;
/// Translate whatever the keyboard speaks into scancode set 1.
const CONFIG_TRANSLATE: u8 = 1 << 6;

/// Device commands, and the byte a device answers one with.
const SET_DEFAULTS: u8 = 0xf6;
const ENABLE_REPORTING: u8 = 0xf4;
const SET_RATE: u8 = 0xf3;
const GET_ID: u8 = 0xf2;
const ACK: u8 = 0xfa;

/// How many times to look at the status register before giving up on it.
/// Each look is a system call, so this is milliseconds, not microseconds.
const PATIENCE: u32 = 100_000;

struct Controller {
    data: Ports,
    command: Ports,
}

impl Controller {
    fn status(&self) -> u8 {
        self.command.try_read8(0).unwrap_or(0)
    }

    /// Wait until the controller will take a byte. False if it never does.
    fn ready_for_input(&self) -> bool {
        (0..PATIENCE).any(|_| self.status() & INPUT_FULL == 0)
    }

    /// Wait for a byte and take it, if one comes.
    fn read(&self) -> Option<u8> {
        (0..PATIENCE)
            .any(|_| self.status() & OUTPUT_FULL != 0)
            .then(|| self.data.try_read8(0).ok())
            .flatten()
    }

    fn command(&self, byte: u8) -> bool {
        self.ready_for_input() && self.command.try_write8(0, byte).is_ok()
    }

    fn write_data(&self, byte: u8) -> bool {
        self.ready_for_input() && self.data.try_write8(0, byte).is_ok()
    }

    /// Send a byte to the mouse and see whether it acknowledges.
    fn to_mouse(&self, byte: u8) -> bool {
        self.command(TO_MOUSE) && self.write_data(byte) && self.read() == Some(ACK)
    }

    /// Throw away anything the controller is holding.
    fn drain(&self) {
        for _ in 0..64 {
            if self.status() & OUTPUT_FULL == 0 {
                return;
            }
            let _ = self.data.try_read8(0);
        }
    }

    fn config(&self) -> Option<u8> {
        self.command(READ_CONFIG).then(|| self.read()).flatten()
    }

    fn set_config(&self, config: u8) -> bool {
        self.command(WRITE_CONFIG) && self.write_data(config)
    }
}

fn main() {
    corrode_rt::log::set(LOG);
    let controller = Controller {
        data: Ports(DATA),
        command: Ports(COMMAND),
    };

    // Quiet first: both devices off, interrupts off, nothing in flight, so
    // no byte that arrives during setup can be mistaken for an answer.
    controller.command(DISABLE_KEYBOARD);
    controller.command(DISABLE_MOUSE);
    controller.drain();
    let Some(config) = controller.config() else {
        println!("no answer from the controller");
        let _ = send(EVENTS, Message::new(LABEL_READY, [0, 0, 0, 0]));
        corrode_rt::exit();
    };
    let quiet = (config & !(CONFIG_KEYBOARD_IRQ | CONFIG_MOUSE_IRQ)) | CONFIG_TRANSLATE;
    controller.set_config(quiet);

    controller.command(ENABLE_KEYBOARD);
    controller.command(ENABLE_MOUSE);
    // A mouse starts silent. Defaults first, so a mouse some firmware left
    // in an odd mode is in the plain three-byte one, then ask it to talk.
    let mouse = controller.to_mouse(SET_DEFAULTS);
    // The IntelliMouse knock: sample rates 200, 100, 80 in a row, after
    // which a mouse with a wheel says it is ID 3 and sends four bytes.
    let wheel = mouse
        && [200, 100, 80]
            .iter()
            .all(|rate| controller.to_mouse(SET_RATE) && controller.to_mouse(*rate))
        && controller.to_mouse(GET_ID)
        && controller.read() == Some(3);
    let mouse = mouse && controller.to_mouse(ENABLE_REPORTING);
    controller.drain();

    // Interrupts on for both, and both ports' clocks running.
    let live = (quiet | CONFIG_KEYBOARD_IRQ | CONFIG_MOUSE_IRQ)
        & !(CONFIG_KEYBOARD_OFF | CONFIG_MOUSE_OFF);
    let keyboard = controller.set_config(live) && controller.config() == Some(live);
    println!(
        "keyboard {}, mouse {}",
        if keyboard { "up" } else { "not answering" },
        match (mouse, wheel) {
            (true, true) => "up with a wheel",
            (true, false) => "up",
            _ => "not answering",
        }
    );
    let _ = send(
        EVENTS,
        Message::new(LABEL_READY, [keyboard as u64, mouse as u64, 0, 0]),
    );

    let irq = Interrupt(IRQ);
    let mut keys = Keyboard::new();
    let mut pointer = if wheel { Mouse::with_wheel() } else { Mouse::new() };
    loop {
        if irq.wait().is_err() {
            println!("cannot wait for the interrupt; stopping");
            corrode_rt::exit();
        }
        // One interrupt can stand for several bytes, and bytes can arrive
        // while these are read: take everything there is.
        for _ in 0..256 {
            let status = controller.status();
            if status & OUTPUT_FULL == 0 {
                break;
            }
            let Ok(byte) = controller.data.try_read8(0) else {
                break;
            };
            if status & FROM_MOUSE != 0 {
                if let Some(event) = pointer.feed(byte) {
                    let _ = send(
                        EVENTS,
                        Message::new(
                            LABEL_POINTER,
                            [
                                event.dx as i64 as u64,
                                event.dy as i64 as u64,
                                event.buttons as u64,
                                event.wheel as i64 as u64,
                            ],
                        ),
                    );
                }
            } else if let Some(event) = keys.feed(byte) {
                let _ = send(
                    EVENTS,
                    Message::new(
                        LABEL_KEY,
                        [
                            event.key.0 as u64,
                            event.pressed as u64,
                            event.text.map_or(0, |c| c as u64),
                            keys.modifiers() as u64,
                        ],
                    ),
                );
            }
        }
    }
}
