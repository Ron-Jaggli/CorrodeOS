//! A driver for an xHCI USB host controller, and the keyboards and pointers
//! plugged into it.
//!
//! An ordinary ring 3 program holding six capabilities, listed in
//! `corrode_abi::xhci`: the controller's configuration space and its
//! registers, its interrupt, a block of memory for everything the controller
//! reads and writes on its own, an endpoint to the compositor and one to the
//! log. It can reach no other device, and the controller can reach only the
//! memory this program was given and told it about.
//!
//! The controller works from structures in memory: a ring of commands, a
//! ring of events it writes back, a table of device contexts, and a ring of
//! transfers per endpoint. Every one fits in a page, so nothing needs pages
//! that are next to each other, and the memory is laid out a page at a time
//! (see [`page`]).
//!
//! Register offsets and structure layouts are those of the eXtensible Host
//! Controller Interface specification, revision 1.2; section numbers below
//! are its.

#![no_std]
#![no_main]

use corrode_rt::abi::ps2::{LABEL_KEY, LABEL_POINTER, LABEL_POINTER_AT};
use corrode_rt::abi::xhci::{BAR, DEVICE, EVENTS, IRQ, LOG, MEMORY, MEMORY_PAGES};
use corrode_rt::device::{Config, Interrupt, Mmio};
use corrode_input::usb::{find_hid, BootKeyboard, HidInterface, Layout, BOOT, KEYBOARD, MOUSE};
use corrode_input::Keyboard;
use corrode_rt::{println, send, Error, Message};

corrode_rt::entry!(main);

const PAGE: usize = 4096;

/// Where each structure is in the memory, by page.
mod page {
    /// The device context base address array: one pointer a slot (6.1).
    pub const CONTEXTS: usize = 0;
    pub const COMMANDS: usize = 1;
    pub const EVENTS: usize = 2;
    /// The event ring segment table: one segment, the page above (6.5).
    pub const SEGMENTS: usize = 3;
    /// The scratchpad buffer array, if the controller wants scratchpads.
    pub const SCRATCHPAD_ARRAY: usize = 4;
    /// Then five pages for each device, in this order.
    pub const DEVICES: usize = 5;
    pub const PER_DEVICE: usize = 5;
    pub const INPUT: usize = 0;
    pub const OUTPUT: usize = 1;
    pub const CONTROL: usize = 2;
    pub const INTERRUPT: usize = 3;
    /// Control transfers' data from byte 0, reports from [`REPORTS`].
    pub const BUFFERS: usize = 4;
    pub const REPORTS: usize = 2048;
}

/// Devices this driver will look after.
const MOST_DEVICES: usize = 4;
/// The scratchpad buffers come after the devices' pages.
const SCRATCHPADS: usize = page::DEVICES + MOST_DEVICES * page::PER_DEVICE;

/// Capability registers (5.3).
const CAPLENGTH: usize = 0x00;
const HCSPARAMS1: usize = 0x04;
const HCSPARAMS2: usize = 0x08;
const HCCPARAMS1: usize = 0x10;
const DBOFF: usize = 0x14;
const RTSOFF: usize = 0x18;

/// Operational registers (5.4), from CAPLENGTH.
const USBCMD: usize = 0x00;
const USBSTS: usize = 0x04;
const CRCR: usize = 0x18;
const DCBAAP: usize = 0x30;
const CONFIG: usize = 0x38;

/// A port's status and control (5.4.8), one a port, every 16 bytes.
const PORTSC: usize = 0x400;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTERRUPTS: u32 = 1 << 2;
const STS_HALTED: u32 = 1 << 0;
/// An interrupter has an event: write 1 to clear.
const STS_INTERRUPT: u32 = 1 << 3;
const STS_NOT_READY: u32 = 1 << 11;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_RESET_CHANGED: u32 = 1 << 21;
/// The change bits, each cleared by writing 1 to it.
const PORT_CHANGES: u32 = 0x00fe_0000;
/// The bits that keep their value when written back as read: power, link
/// state (ignored without its strobe), indicators, wake enables. The rest
/// are written as 0, which leaves them alone -- writing PED as 1 would
/// disable the port.
const PORT_KEEP: u32 = 0x0e00_c3e0;

/// Interrupter 0's registers (5.5.2), from the runtime base.
const IMAN: usize = 0x20;
const IMOD: usize = 0x24;
const ERSTSZ: usize = 0x28;
const ERSTBA: usize = 0x30;
const ERDP: usize = 0x38;
/// Interrupt pending (write 1 to clear) and enabled.
const IMAN_PENDING: u32 = 1 << 0;
const IMAN_ENABLE: u32 = 1 << 1;
/// Event handler busy: written as 1 with the dequeue pointer, to clear it.
const ERDP_BUSY: u64 = 1 << 3;

/// Transfer request blocks (6.4): sixteen bytes, a ring of them to a page,
/// the last a link back to the first.
const TRBS: usize = PAGE / 16;

/// TRB types.
mod trb {
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    pub const LINK: u32 = 6;
    pub const ENABLE_SLOT: u32 = 9;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    pub const EVALUATE_CONTEXT: u32 = 13;
    pub const NO_OP_COMMAND: u32 = 23;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
}
const CYCLE: u32 = 1 << 0;
/// In a link TRB: flip the cycle bit when following it.
const TOGGLE_CYCLE: u32 = 1 << 1;
/// In a transfer TRB: say so with an event if fewer bytes came than asked.
const INTERRUPT_ON_SHORT: u32 = 1 << 2;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
/// In a setup TRB: the eight bytes of the request are the parameter itself.
const IMMEDIATE: u32 = 1 << 6;
/// Completion codes (6.4.5).
const SUCCESS: u32 = 1;
const SHORT_PACKET: u32 = 13;

/// Standard requests (USB 2.0 9.4).
const GET_DESCRIPTOR: u8 = 6;
const SET_CONFIGURATION: u8 = 9;
const DEVICE_DESCRIPTOR: u16 = 0x0100;
const CONFIGURATION_DESCRIPTOR: u16 = 0x0200;
/// A HID report descriptor, asked of the interface (HID 1.11 7.1.1).
const REPORT_DESCRIPTOR: u16 = 0x2200;
const FROM_INTERFACE: u8 = 0x81;
/// HID class requests (HID 1.11 7.2): which protocol, boot or report.
const SET_PROTOCOL: u8 = 0x0b;
const BOOT_PROTOCOL: u16 = 0;
/// Requests' first byte: which way, whose, to what.
const TO_DEVICE: u8 = 0x00;
const FROM_DEVICE: u8 = 0x80;
const CLASS_TO_INTERFACE: u8 = 0x21;
/// Reports waiting for a device at once: each its own TRB and buffer.
const REPORTS_QUEUED: usize = 16;
/// The most of a report read: the most a full-speed interrupt endpoint
/// sends in a packet.
const MOST_REPORT: usize = 64;
/// The most of a configuration descriptor read.
const MOST_CONFIG: usize = 1024;

/// A TRB as four words: a 64-bit parameter, a status word, a control word
/// holding the type in bits 10 to 15 and the cycle bit in bit 0.
#[derive(Clone, Copy, Debug, Default)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    /// An event's completion code.
    fn code(&self) -> u32 {
        self.status >> 24
    }

    /// An event's slot.
    fn slot(&self) -> usize {
        (self.control >> 24) as usize
    }
}

/// The memory the controller works in, mapped.
struct Pages {
    base: *mut u8,
    len: usize,
    physical: [u64; MEMORY_PAGES],
}

impl Pages {
    fn map() -> Result<Pages, Error> {
        let mapping = corrode_rt::map(MEMORY)?;
        let mut physical = [0; MEMORY_PAGES];
        for (page, address) in physical.iter_mut().enumerate() {
            *address = corrode_rt::physical(MEMORY, page)?;
        }
        Ok(Pages {
            base: mapping.address,
            len: mapping.len,
            physical,
        })
    }

    /// The address of byte `offset` of page `page`, for the controller.
    fn physical(&self, page: usize, offset: usize) -> u64 {
        self.physical[page] + offset as u64
    }

    fn at(&self, page: usize, offset: usize) -> *mut u8 {
        assert!(page * PAGE + offset < self.len && offset < PAGE);
        // SAFETY: inside the mapping, just checked.
        unsafe { self.base.add(page * PAGE + offset) }
    }

    fn read32(&self, page: usize, offset: usize) -> u32 {
        // SAFETY: inside the mapping; volatile, since the controller writes
        // this memory on its own.
        unsafe { self.at(page, offset).cast::<u32>().read_volatile() }
    }

    fn write32(&self, page: usize, offset: usize, value: u32) {
        // SAFETY: as for `read32`.
        unsafe { self.at(page, offset).cast::<u32>().write_volatile(value) }
    }

    fn read8(&self, page: usize, offset: usize) -> u8 {
        // SAFETY: as for `read32`.
        unsafe { self.at(page, offset).read_volatile() }
    }

    fn write64(&self, page: usize, offset: usize, value: u64) {
        self.write32(page, offset, value as u32);
        self.write32(page, offset + 4, (value >> 32) as u32);
    }

    fn trb(&self, page: usize, index: usize) -> Trb {
        let at = index * 16;
        Trb {
            parameter: self.read32(page, at) as u64 | (self.read32(page, at + 4) as u64) << 32,
            status: self.read32(page, at + 8),
            control: self.read32(page, at + 12),
        }
    }

    /// Write a TRB, its control word -- which holds the cycle bit that hands
    /// it to the controller -- last.
    fn put_trb(&self, page: usize, index: usize, trb: Trb) {
        let at = index * 16;
        self.write64(page, at, trb.parameter);
        self.write32(page, at + 8, trb.status);
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.write32(page, at + 12, trb.control);
    }

    fn zero(&self, page: usize) {
        for offset in (0..PAGE).step_by(4) {
            self.write32(page, offset, 0);
        }
    }
}

/// A ring the driver fills and the controller empties: commands, or one
/// endpoint's transfers.
struct Ring {
    page: usize,
    index: usize,
    cycle: bool,
}

impl Ring {
    fn new(pages: &Pages, page: usize) -> Ring {
        pages.zero(page);
        Ring {
            page,
            index: 0,
            cycle: true,
        }
    }

    /// Where the controller should start: the ring's first TRB, with the
    /// cycle bit it should expect.
    fn start(&self, pages: &Pages) -> u64 {
        pages.physical(self.page, 0) | 1
    }

    /// Put `trb` on the ring and say where it went, for matching its
    /// completion. At the end of the page, a link back to the start.
    fn push(&mut self, pages: &Pages, mut trb: Trb) -> u64 {
        trb.control = (trb.control & !CYCLE) | self.cycle as u32;
        let at = pages.physical(self.page, self.index * 16);
        pages.put_trb(self.page, self.index, trb);
        self.index += 1;
        if self.index == TRBS - 1 {
            let link = Trb {
                parameter: pages.physical(self.page, 0),
                status: 0,
                control: trb::LINK << 10 | TOGGLE_CYCLE | self.cycle as u32,
            };
            pages.put_trb(self.page, self.index, link);
            self.index = 0;
            self.cycle = !self.cycle;
        }
        at
    }
}

/// A device with an address: its slot, the first of its pages, and its
/// control endpoint.
struct Device {
    slot: usize,
    page: usize,
    port: usize,
    speed: u32,
    control: Ring,
    max_packet: u16,
}

/// A device being read: its interrupt endpoint's ring, and what makes
/// sense of its reports.
struct Input {
    device: Device,
    /// The endpoint's device context index: its doorbell target.
    target: usize,
    ring: Ring,
    reader: Reader,
    /// Whether a report has come yet: the first is logged.
    heard: bool,
}

enum Reader {
    /// Boot reports, said as set-1 scancodes to the same decoder the PS/2
    /// driver uses.
    Keyboard(BootKeyboard, Keyboard),
    /// A mouse or a tablet, its reports laid out as this says.
    Pointer(Layout),
}

/// Port speeds, as PORTSC and the slot context give them.
const LOW_SPEED: u32 = 2;
const HIGH_SPEED: u32 = 3;
const SUPER_SPEED: u32 = 4;

/// The controller, running.
struct Controller {
    registers: Mmio,
    operational: usize,
    ports: usize,
    /// Bytes in each context of a device context: 32 or 64.
    context: usize,
    runtime: usize,
    doorbells: usize,
    pages: Pages,
    commands: Ring,
    /// Where the next event will be, and the cycle bit it will have.
    event: usize,
    event_cycle: bool,
}

impl Controller {
    fn op32(&self, register: usize) -> u32 {
        self.registers.read32(self.operational + register)
    }

    fn set_op32(&self, register: usize, value: u32) {
        self.registers.write32(self.operational + register, value)
    }

    fn set_op64(&self, register: usize, value: u64) {
        self.registers.write32(self.operational + register, value as u32);
        self.registers.write32(self.operational + register + 4, (value >> 32) as u32);
    }

    fn ring(&self, doorbell: usize, target: u32) {
        self.registers.write32(self.doorbells + 4 * doorbell, target);
    }

    /// The next event, if the controller has written one; and tell it the
    /// slot is free.
    fn next_event(&mut self) -> Option<Trb> {
        let event = self.pages.trb(page::EVENTS, self.event);
        if (event.control & CYCLE != 0) != self.event_cycle {
            return None;
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        self.event += 1;
        if self.event == TRBS {
            self.event = 0;
            self.event_cycle = !self.event_cycle;
        }
        let dequeue = self.pages.physical(page::EVENTS, self.event * 16) | ERDP_BUSY;
        self.registers.write32(self.runtime + ERDP, dequeue as u32);
        self.registers.write32(self.runtime + ERDP + 4, (dequeue >> 32) as u32);
        Some(event)
    }

    /// Wait up to `ms` for an event `wanted` says is the one, setting aside
    /// (and logging) anything else.
    fn wait_for(&mut self, ms: u64, wanted: impl Fn(&Trb) -> bool) -> Option<Trb> {
        let deadline = corrode_rt::now_ns() + ms * 1_000_000;
        loop {
            while let Some(event) = self.next_event() {
                if wanted(&event) {
                    return Some(event);
                }
                if event.kind() != trb::PORT_STATUS_CHANGE {
                    println!("an event not waited for: type {}, code {}", event.kind(), event.code());
                }
            }
            if corrode_rt::now_ns() > deadline {
                return None;
            }
            corrode_rt::yield_now();
        }
    }

    /// Run one command and wait for it to complete: its completion event.
    fn command(&mut self, trb: Trb) -> Option<Trb> {
        let at = self.commands.push(&self.pages, trb);
        self.ring(0, 0);
        self.wait_for(500, |event| event.kind() == trb::COMMAND_COMPLETION && event.parameter == at)
    }

    /// Run a command that must succeed, saying which did not.
    fn must(&mut self, what: &str, trb: Trb) -> Option<Trb> {
        match self.command(trb) {
            Some(event) if event.code() == SUCCESS => Some(event),
            Some(event) => {
                println!("{what} failed: code {}", event.code());
                None
            }
            None => {
                println!("{what} was never answered");
                None
            }
        }
    }

    fn portsc(&self, port: usize) -> u32 {
        self.op32(PORTSC + 16 * port)
    }

    fn set_portsc(&self, port: usize, value: u32) {
        self.set_op32(PORTSC + 16 * port, value)
    }

    /// Bring up port `port` (from 0) if something is plugged into it, and
    /// say how fast it is. A USB 2 port is enabled by resetting it; a USB 3
    /// port enables itself.
    fn reset_port(&self, port: usize) -> Option<u32> {
        let status = self.portsc(port);
        if status & PORT_CONNECTED == 0 {
            return None;
        }
        if status & PORT_ENABLED == 0 {
            self.set_portsc(port, status & PORT_KEEP | PORT_RESET);
            if !until(200, || self.portsc(port) & PORT_RESET_CHANGED != 0) {
                println!("port {}: the reset never finished", port + 1);
                return None;
            }
        }
        let status = self.portsc(port);
        self.set_portsc(port, status & PORT_KEEP | status & PORT_CHANGES);
        (status & PORT_ENABLED != 0).then_some((status >> 10) & 0xf)
    }

    /// Write a device's control endpoint into its input context (6.2.3).
    fn control_endpoint(&self, device: &Device) {
        let at = 2 * self.context;
        let input = device.page + page::INPUT;
        // Three errors before giving up; a control endpoint (type 4).
        self.pages.write32(input, at + 4, 3 << 1 | 4 << 3 | (device.max_packet as u32) << 16);
        self.pages.write64(input, at + 8, device.control.start(&self.pages));
        // Average TRB length: a setup packet's.
        self.pages.write32(input, at + 16, 8);
    }

    /// Give the device on `port` a slot and an address (4.3), its pages
    /// starting at `page`.
    fn address(&mut self, port: usize, speed: u32, page: usize) -> Option<Device> {
        let enabled = self.must("Enable Slot", Trb { control: trb::ENABLE_SLOT << 10, ..Trb::default() })?;
        let slot = enabled.slot();
        // The context table has room for the slots enabled, and no more.
        if !(1..=MOST_DEVICES).contains(&slot) {
            println!("Enable Slot gave slot {slot}, which was never enabled");
            return None;
        }
        for offset in [page::INPUT, page::OUTPUT, page::BUFFERS] {
            self.pages.zero(page + offset);
        }
        let device = Device {
            slot,
            page,
            port,
            speed,
            control: Ring::new(&self.pages, page + page::CONTROL),
            // Until the device says otherwise: the most a control endpoint
            // may have at this speed, or the only size at low.
            max_packet: match speed {
                LOW_SPEED => 8,
                HIGH_SPEED => 64,
                SUPER_SPEED.. => 512,
                _ => 64,
            },
        };
        // The input context: add the slot context and endpoint 0's (6.2.5).
        let input = page + page::INPUT;
        self.pages.write32(input, 4, 0b11);
        // One context entry; the speed; the root hub port, from 1.
        self.pages.write32(input, self.context, speed << 20 | 1 << 27);
        self.pages.write32(input, self.context + 4, ((port + 1) as u32) << 16);
        self.control_endpoint(&device);
        self.pages.write64(page::CONTEXTS, slot * 8, self.pages.physical(page + page::OUTPUT, 0));
        self.must(
            "Address Device",
            Trb {
                parameter: self.pages.physical(input, 0),
                status: 0,
                control: trb::ADDRESS_DEVICE << 10 | (slot as u32) << 24,
            },
        )?;
        Some(device)
    }

    /// Make a control request (USB 2.0 9.3). What a device sends back is in
    /// its buffer page from byte 0.
    fn control(
        &mut self,
        device: &mut Device,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Option<()> {
        let setup = request_type as u64
            | (request as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (length as u64) << 48;
        let reading = request_type & FROM_DEVICE != 0;
        // The transfer type: no data stage, data out, data in.
        let stages = match (length, reading) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        device.control.push(
            &self.pages,
            Trb { parameter: setup, status: 8, control: trb::SETUP << 10 | IMMEDIATE | stages << 16 },
        );
        if length > 0 {
            device.control.push(
                &self.pages,
                Trb {
                    parameter: self.pages.physical(device.page + page::BUFFERS, 0),
                    status: length as u32,
                    control: trb::DATA << 10 | (reading as u32) << 16,
                },
            );
        }
        // The status stage goes the other way from the data; in if none.
        let status_in = length == 0 || !reading;
        let at = device.control.push(
            &self.pages,
            Trb {
                parameter: 0,
                status: 0,
                control: trb::STATUS << 10 | INTERRUPT_ON_COMPLETION | (status_in as u32) << 16,
            },
        );
        self.ring(device.slot, 1);
        let slot = device.slot;
        let done = |code: u32| matches!(code, SUCCESS | SHORT_PACKET);
        // The status stage's event, or the event of a stage that failed.
        let event = self.wait_for(500, |event| {
            event.kind() == trb::TRANSFER_EVENT && event.slot() == slot && (event.parameter == at || !done(event.code()))
        });
        match event {
            Some(event) if event.parameter == at && done(event.code()) => Some(()),
            Some(event) => {
                println!("slot {slot}: request {request} failed: code {}", event.code());
                None
            }
            None => {
                println!("slot {slot}: request {request} was never answered");
                None
            }
        }
    }

    /// The first `len` bytes of a device's buffer page.
    fn read<const N: usize>(&self, device: &Device, len: usize) -> ([u8; N], usize) {
        let mut bytes = [0; N];
        let len = len.min(N);
        for (offset, byte) in bytes[..len].iter_mut().enumerate() {
            *byte = self.pages.read8(device.page + page::BUFFERS, offset);
        }
        (bytes, len)
    }

    /// Address the device on `port`, read its descriptors, and configure
    /// it if it has a HID interface: what is needed to start reading its
    /// reports.
    fn enumerate(&mut self, port: usize, speed: u32, page: usize) -> Option<(Device, HidInterface)> {
        let mut device = self.address(port, speed, page)?;
        // The first eight bytes of the device descriptor say how large the
        // control endpoint's packets really are (at super speed, as a power
        // of two, and always 512).
        self.control(&mut device, FROM_DEVICE, GET_DESCRIPTOR, DEVICE_DESCRIPTOR, 0, 8)?;
        let (first, _) = self.read::<8>(&device, 8);
        let max_packet = first[7] as u16;
        if speed < SUPER_SPEED && max_packet >= 8 && max_packet != device.max_packet {
            device.max_packet = max_packet;
            let input = device.page + page::INPUT;
            self.pages.write32(input, 0, 0);
            self.pages.write32(input, 4, 0b10);
            self.control_endpoint(&device);
            self.must(
                "Evaluate Context",
                Trb {
                    parameter: self.pages.physical(input, 0),
                    status: 0,
                    control: trb::EVALUATE_CONTEXT << 10 | (device.slot as u32) << 24,
                },
            )?;
        }
        self.control(&mut device, FROM_DEVICE, GET_DESCRIPTOR, DEVICE_DESCRIPTOR, 0, 18)?;
        let (descriptor, _) = self.read::<18>(&device, 18);
        let vendor = u16::from_le_bytes([descriptor[8], descriptor[9]]);
        let product = u16::from_le_bytes([descriptor[10], descriptor[11]]);

        // The configuration's first nine bytes say how long all of it is.
        self.control(&mut device, FROM_DEVICE, GET_DESCRIPTOR, CONFIGURATION_DESCRIPTOR, 0, 9)?;
        let (head, _) = self.read::<9>(&device, 9);
        let total = (u16::from_le_bytes([head[2], head[3]]) as usize).clamp(9, MOST_CONFIG);
        self.control(&mut device, FROM_DEVICE, GET_DESCRIPTOR, CONFIGURATION_DESCRIPTOR, 0, total as u16)?;
        let (config, len) = self.read::<MOST_CONFIG>(&device, total);
        let Some(hid) = find_hid(&config[..len]) else {
            println!("port {}: {vendor:04x}:{product:04x}, not a keyboard or a pointer; left alone", port + 1);
            return None;
        };
        self.control(&mut device, TO_DEVICE, SET_CONFIGURATION, hid.configuration as u16, 0, 0)?;
        let what = match (hid.subclass, hid.protocol) {
            (BOOT, KEYBOARD) => "a keyboard",
            (BOOT, MOUSE) => "a mouse",
            // What it is, its report descriptor says.
            _ => "a HID device",
        };
        println!(
            "port {}: {vendor:04x}:{product:04x} at {}, {what}: interface {}, endpoint {} in, {}-byte packets",
            port + 1,
            match speed {
                LOW_SPEED => "low speed",
                HIGH_SPEED => "high speed",
                SUPER_SPEED.. => "super speed",
                _ => "full speed",
            },
            hid.interface,
            hid.endpoint,
            hid.max_packet,
        );
        Some((device, hid))
    }

    /// How a report-protocol device's reports are laid out, from its
    /// report descriptor: `None` if it has no X and Y, or makes no sense.
    fn layout(&mut self, device: &mut Device, hid: &HidInterface) -> Option<Layout> {
        let len = (hid.report_length as usize).clamp(1, MOST_CONFIG);
        self.control(device, FROM_INTERFACE, GET_DESCRIPTOR, REPORT_DESCRIPTOR, hid.interface as u16, len as u16)?;
        let (descriptor, len) = self.read::<MOST_CONFIG>(device, len);
        Layout::parse(&descriptor[..len])
    }

    /// Set up the device's interrupt IN endpoint (Configure Endpoint,
    /// 4.6.6) and queue reads on it.
    fn start(&mut self, mut device: Device, hid: &HidInterface, reader: Reader) -> Option<Input> {
        if hid.subclass == BOOT {
            // The boot protocol's reports are the same on every device;
            // the report protocol's are whatever the device likes.
            self.control(
                &mut device,
                CLASS_TO_INTERFACE,
                SET_PROTOCOL,
                BOOT_PROTOCOL,
                hid.interface as u16,
                0,
            )?;
        }
        // An IN endpoint's context index is twice its number, plus one.
        let target = hid.endpoint as usize * 2 + 1;
        let ring = Ring::new(&self.pages, device.page + page::INTERRUPT);
        let input = device.page + page::INPUT;
        self.pages.zero(input);
        // Add the slot context, whose last entry is now this endpoint, and
        // the endpoint's.
        self.pages.write32(input, 4, 1 | 1 << target);
        self.pages.write32(input, self.context, device.speed << 20 | (target as u32) << 27);
        self.pages.write32(input, self.context + 4, ((device.port + 1) as u32) << 16);
        // How often: a power of two of 125 us. At high speed and above the
        // descriptor gives that power, plus one; below, milliseconds.
        let interval = if device.speed >= HIGH_SPEED {
            hid.interval.clamp(1, 16) as u32 - 1
        } else {
            (hid.interval.max(1) as u32 * 8).ilog2().clamp(3, 10)
        };
        let packet = (hid.max_packet as usize).min(MOST_REPORT) as u32;
        let at = (1 + target) * self.context;
        self.pages.write32(input, at, interval << 16);
        // Three errors; an interrupt IN endpoint (type 7).
        self.pages.write32(input, at + 4, 3 << 1 | 7 << 3 | packet << 16);
        self.pages.write64(input, at + 8, ring.start(&self.pages));
        // Average TRB length and most bytes an interval: one packet.
        self.pages.write32(input, at + 16, packet | packet << 16);
        self.must(
            "Configure Endpoint",
            Trb {
                parameter: self.pages.physical(input, 0),
                status: 0,
                control: trb::CONFIGURE_ENDPOINT << 10 | (device.slot as u32) << 24,
            },
        )?;
        let mut input = Input {
            device,
            target,
            ring,
            reader,
            heard: false,
        };
        for index in 0..REPORTS_QUEUED {
            let buffer = self.pages.physical(input.device.page + page::BUFFERS, page::REPORTS + index * MOST_REPORT);
            self.read_report(&mut input, buffer, packet);
        }
        self.ring(input.device.slot, input.target as u32);
        Some(input)
    }

    /// Queue one read of a report into `buffer`.
    fn read_report(&mut self, input: &mut Input, buffer: u64, len: u32) {
        input.ring.push(
            &self.pages,
            Trb {
                parameter: buffer,
                status: len,
                control: trb::NORMAL << 10 | INTERRUPT_ON_SHORT | INTERRUPT_ON_COMPLETION,
            },
        );
    }

    /// A report came in, or a read failed: make sense of it, and read again
    /// into the same buffer.
    fn transfer(&mut self, inputs: &mut [Option<Input>], event: &Trb) {
        let slot = event.slot();
        let target = ((event.control >> 16) & 0x1f) as usize;
        let Some(input) = inputs
            .iter_mut()
            .flatten()
            .find(|input| input.device.slot == slot && input.target == target)
        else {
            return;
        };
        if !matches!(event.code(), SUCCESS | SHORT_PACKET) {
            // A stalled or broken endpoint stays stopped: nothing more is
            // queued on it, and the device falls silent.
            println!("slot {slot}: a report failed: code {}", event.code());
            return;
        }
        // The TRB the event is about still says which buffer it filled.
        let ring = self.pages.physical(input.ring.page, 0);
        let Some(index) = event.parameter.checked_sub(ring).map(|at| at as usize / 16).filter(|i| *i < TRBS) else {
            return;
        };
        let read = self.pages.trb(input.ring.page, index);
        let buffers = self.pages.physical(input.device.page + page::BUFFERS, 0);
        let Some(offset) = read
            .parameter
            .checked_sub(buffers)
            .map(|offset| offset as usize)
            .filter(|offset| (page::REPORTS..PAGE).contains(offset) && offset % MOST_REPORT == 0)
        else {
            return;
        };
        let asked = read.status & 0x1ffff;
        let len = asked.saturating_sub(event.status & 0xff_ffff) as usize;
        if !input.heard {
            input.heard = true;
            println!("slot {slot}: the first report, {len} bytes");
        }
        let mut report = [0u8; MOST_REPORT];
        for (at, byte) in report[..len.min(MOST_REPORT)].iter_mut().enumerate() {
            *byte = self.pages.read8(input.device.page + page::BUFFERS, offset + at);
        }
        match &mut input.reader {
            Reader::Keyboard(boot, keyboard) => boot.report(&report[..len], &mut |byte| {
                if let Some(event) = keyboard.feed(byte) {
                    let _ = send(
                        EVENTS,
                        Message::new(
                            LABEL_KEY,
                            [
                                event.key.0 as u64,
                                event.pressed as u64,
                                event.text.map_or(0, |c| c as u64),
                                keyboard.modifiers() as u64,
                            ],
                        ),
                    );
                }
            }),
            Reader::Pointer(layout) => {
                if let Some(pointer) = layout.decode(&report[..len]) {
                    let label = if pointer.absolute { LABEL_POINTER_AT } else { LABEL_POINTER };
                    let _ = send(
                        EVENTS,
                        Message::new(
                            label,
                            [
                                pointer.x as i64 as u64,
                                pointer.y as i64 as u64,
                                pointer.buttons as u64,
                                pointer.wheel as i64 as u64,
                            ],
                        ),
                    );
                }
            }
        }
        self.read_report(input, read.parameter, asked);
        self.ring(input.device.slot, input.target as u32);
    }
}

/// Wait up to `ms` for `done` to say so.
fn until(ms: u64, done: impl Fn() -> bool) -> bool {
    let deadline = corrode_rt::now_ns() + ms * 1_000_000;
    while !done() {
        if corrode_rt::now_ns() > deadline {
            return false;
        }
        corrode_rt::yield_now();
    }
    true
}

/// Reset the controller and set up what it needs to run: the context
/// table, the command ring, the event ring (4.2).
fn bring_up(registers: Mmio) -> Result<Controller, &'static str> {
    let operational = registers.read8(CAPLENGTH) as usize;
    let params1 = registers.read32(HCSPARAMS1);
    let params2 = registers.read32(HCSPARAMS2);
    let slots = (params1 & 0xff) as usize;
    let ports = (params1 >> 24) as usize;
    let scratchpads = ((params2 >> 27) & 0x1f | ((params2 >> 21) & 0x1f) << 5) as usize;
    let wide_contexts = registers.read32(HCCPARAMS1) & (1 << 2) != 0;
    let doorbells = (registers.read32(DBOFF) & !0x3) as usize;
    let runtime = (registers.read32(RTSOFF) & !0x1f) as usize;
    println!(
        "controller: {slots} slots, {ports} ports, {scratchpads} scratchpads, {}-byte contexts",
        if wide_contexts { 64 } else { 32 }
    );
    if SCRATCHPADS + scratchpads > MEMORY_PAGES {
        return Err("it wants more scratchpad pages than there are");
    }

    let op = |register: usize| registers.read32(operational + register);
    // Stopped, then reset: the state the firmware left it in is not ours.
    registers.write32(operational + USBCMD, op(USBCMD) & !CMD_RUN);
    if !until(100, || op(USBSTS) & STS_HALTED != 0) {
        return Err("it would not stop");
    }
    registers.write32(operational + USBCMD, CMD_RESET);
    if !until(500, || op(USBCMD) & CMD_RESET == 0 && op(USBSTS) & STS_NOT_READY == 0) {
        return Err("it would not reset");
    }

    let pages = Pages::map().map_err(|_| "its memory would not map")?;
    pages.zero(page::CONTEXTS);
    pages.zero(page::EVENTS);
    pages.zero(page::SEGMENTS);
    if scratchpads > 0 {
        pages.zero(page::SCRATCHPAD_ARRAY);
        for index in 0..scratchpads {
            pages.zero(SCRATCHPADS + index);
            pages.write64(page::SCRATCHPAD_ARRAY, index * 8, pages.physical(SCRATCHPADS + index, 0));
        }
        pages.write64(page::CONTEXTS, 0, pages.physical(page::SCRATCHPAD_ARRAY, 0));
    }
    let commands = Ring::new(&pages, page::COMMANDS);
    // One segment of event ring, a page of it (6.5).
    pages.write64(page::SEGMENTS, 0, pages.physical(page::EVENTS, 0));
    pages.write32(page::SEGMENTS, 8, TRBS as u32);

    let controller = Controller {
        registers,
        operational,
        ports,
        context: if wide_contexts { 64 } else { 32 },
        runtime,
        doorbells,
        pages,
        commands,
        event: 0,
        event_cycle: true,
    };
    controller.set_op32(CONFIG, slots.min(MOST_DEVICES) as u32);
    controller.set_op64(DCBAAP, controller.pages.physical(page::CONTEXTS, 0));
    controller.set_op64(CRCR, controller.commands.start(&controller.pages));
    let r = &controller.registers;
    r.write32(runtime + ERSTSZ, 1);
    let dequeue = controller.pages.physical(page::EVENTS, 0);
    r.write32(runtime + ERDP, dequeue as u32);
    r.write32(runtime + ERDP + 4, (dequeue >> 32) as u32);
    let table = controller.pages.physical(page::SEGMENTS, 0);
    r.write32(runtime + ERSTBA, table as u32);
    r.write32(runtime + ERSTBA + 4, (table >> 32) as u32);
    // At most one interrupt a millisecond: 4000 of 250 ns.
    r.write32(runtime + IMOD, 4000);
    r.write32(runtime + IMAN, IMAN_PENDING | IMAN_ENABLE);

    controller.set_op32(USBCMD, CMD_RUN | CMD_INTERRUPTS);
    if !until(100, || controller.op32(USBSTS) & STS_HALTED == 0) {
        return Err("it would not start");
    }
    Ok(controller)
}

fn main() {
    corrode_rt::log::set(LOG);
    let config = Config(DEVICE);
    if config.enable().is_err() {
        println!("cannot turn on bus mastering; stopping");
        return;
    }
    let registers = match Mmio::map(BAR) {
        Ok(registers) => registers,
        Err(error) => {
            println!("no registers: {:?}", error);
            return;
        }
    };
    let mut controller = match bring_up(registers) {
        Ok(controller) => controller,
        Err(why) => {
            println!("the controller did not come up: {why}");
            return;
        }
    };
    // The command ring, tried: a no-op must come back done.
    let noop = controller.command(Trb {
        control: trb::NO_OP_COMMAND << 10,
        ..Trb::default()
    });
    match noop {
        Some(event) if event.code() == SUCCESS => {
            println!("controller up: reset, rings set, a no-op command answered")
        }
        Some(event) => println!("a no-op command failed: code {}", event.code()),
        None => println!("a no-op command was never answered"),
    }

    // Every port with something on it, until there are no more pages. A
    // device that was given a slot keeps its pages even if it is then left
    // alone, since the controller may still write its context.
    let mut inputs: [Option<Input>; MOST_DEVICES] = [const { None }; MOST_DEVICES];
    let mut used = 0;
    let mut found = 0;
    for port in 0..controller.ports {
        if used == MOST_DEVICES {
            break;
        }
        let Some(speed) = controller.reset_port(port) else {
            continue;
        };
        let page = page::DEVICES + used * page::PER_DEVICE;
        used += 1;
        let Some((mut device, hid)) = controller.enumerate(port, speed, page) else {
            continue;
        };
        let (reader, what) = match (hid.subclass, hid.protocol) {
            (BOOT, KEYBOARD) => (Reader::Keyboard(BootKeyboard::new(), Keyboard::new()), "a keyboard's"),
            (BOOT, MOUSE) => (Reader::Pointer(Layout::BOOT_MOUSE), "a mouse's"),
            _ => match controller.layout(&mut device, &hid) {
                Some(layout) if layout.absolute => (Reader::Pointer(layout), "a tablet's"),
                Some(layout) => (Reader::Pointer(layout), "a mouse's"),
                None => {
                    println!("slot {}: neither a keyboard nor a pointer; left alone", device.slot);
                    continue;
                }
            },
        };
        if let Some(input) = controller.start(device, &hid, reader) {
            println!("slot {}: reading {what} reports", input.device.slot);
            inputs[found] = Some(input);
            found += 1;
        }
    }
    println!("{found} USB input devices");

    let irq = Interrupt(IRQ);
    loop {
        if irq.wait().is_err() {
            println!("cannot wait for the interrupt; stopping");
            corrode_rt::exit();
        }
        controller.set_op32(USBSTS, STS_INTERRUPT);
        controller
            .registers
            .write32(controller.runtime + IMAN, IMAN_PENDING | IMAN_ENABLE);
        while let Some(event) = controller.next_event() {
            if event.kind() == trb::TRANSFER_EVENT {
                controller.transfer(&mut inputs, &event);
            }
        }
    }
}
