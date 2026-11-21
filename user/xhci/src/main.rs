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

use corrode_rt::abi::xhci::{BAR, DEVICE, EVENTS, IRQ, LOG, MEMORY, MEMORY_PAGES};
use corrode_rt::device::{Config, Interrupt, Mmio};
use corrode_rt::{println, Error};

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
    /// Then five pages for each device: its input context, its output
    /// context, its control endpoint's ring, its interrupt endpoint's ring,
    /// and a page of buffers.
    pub const DEVICES: usize = 5;
    pub const PER_DEVICE: usize = 5;
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

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTERRUPTS: u32 = 1 << 2;
const STS_HALTED: u32 = 1 << 0;
const STS_NOT_READY: u32 = 1 << 11;

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
    pub const LINK: u32 = 6;
    pub const NO_OP_COMMAND: u32 = 23;
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETION: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
}
const CYCLE: u32 = 1 << 0;
/// In a link TRB: flip the cycle bit when following it.
const TOGGLE_CYCLE: u32 = 1 << 1;
/// Completion codes (6.4.5).
const SUCCESS: u32 = 1;

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

/// The controller, running.
struct Controller {
    registers: Mmio,
    operational: usize,
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

    let _ = (EVENTS, trb::TRANSFER_EVENT, Trb::slot);
    let irq = Interrupt(IRQ);
    loop {
        if irq.wait().is_err() {
            println!("cannot wait for the interrupt; stopping");
            corrode_rt::exit();
        }
        controller
            .registers
            .write32(controller.runtime + IMAN, IMAN_PENDING | IMAN_ENABLE);
        while let Some(event) = controller.next_event() {
            let _ = event;
        }
    }
}
