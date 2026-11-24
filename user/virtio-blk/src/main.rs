//! The settings store: a driver for a virtio block device, in ring 3.
//!
//! The desktop keeps its theme in the disk's first sector, so what is
//! picked in Settings is still there after a reboot. This task drives the
//! disk and answers two calls, load and save; it holds its device, its
//! queue memory, an endpoint to be called on and a log, and nothing else.
//! What it reads from the disk is checked as a theme record before anyone
//! is told it is one, so a blank or foreign disk is just "nothing kept".
//!
//! One request at a time, and the device polled: a request is a sector, a
//! few times in a session. The device protocol is the Virtual I/O Device
//! specification 1.2: 3.1 for bringing a device up, 4.1 for the PCI
//! transport, 5.2 for the block device.

#![no_std]
#![no_main]

use corrode_rt::abi::store;
use corrode_rt::device::{Config, Mmio};
use corrode_rt::{abi, identify, println, recv, reply, Error, Message};
use corrode_theme::{Theme, RECORD_LEN};
use corrode_virtio::blk;
use corrode_virtio::pci::{common, Layout};
use corrode_virtio::queue::{self, Buffer, Queue};
use corrode_virtio::{status, F_VERSION_1};

corrode_rt::entry!(main);

const PAGE: usize = 4096;
/// The request queue is queue 0.
const REQUEST_QUEUE: u16 = 0;

/// How the queue memory is laid out, by page: the ring; the request's
/// header, and its status byte after it; the sector.
const QUEUE_RING: usize = 0;
const QUEUE_HEADER: usize = 1;
const STATUS_AT: usize = 64;
const QUEUE_SECTOR: usize = 2;

/// The sector the theme is kept in.
const THEME_SECTOR: u64 = 0;

/// A memory capability, mapped.
struct Pages {
    base: *mut u8,
    len: usize,
    slot: usize,
}

impl Pages {
    fn map(slot: usize) -> Result<Pages, Error> {
        let mapping = corrode_rt::map(slot)?;
        Ok(Pages {
            base: mapping.address,
            len: mapping.len,
            slot,
        })
    }

    fn page(&self, page: usize) -> *mut u8 {
        assert!(page * PAGE < self.len);
        // SAFETY: inside the mapping, just checked.
        unsafe { self.base.add(page * PAGE) }
    }

    fn physical(&self, page: usize) -> Result<u64, Error> {
        corrode_rt::physical(self.slot, page)
    }

    /// The bytes of `len` starting `offset` into page `page`.
    fn bytes(&mut self, page: usize, offset: usize, len: usize) -> &mut [u8] {
        assert!(offset + len <= PAGE);
        // SAFETY: inside one page of the mapping, ours for the life of the
        // task. The device touches it only between submitting a request
        // and taking its answer, while this slice is not held.
        unsafe { core::slice::from_raw_parts_mut(self.page(page).add(offset), len) }
    }
}

/// The device, set up.
struct Disk {
    bars: [Option<Mmio>; 6],
    common: (usize, usize),
    doorbell: (usize, usize),
    queue: Queue,
    pages: Pages,
}

impl Disk {
    fn mmio(&self, (bar, offset): (usize, usize)) -> (&Mmio, usize) {
        (self.bars[bar].as_ref().expect("mapped in bring_up"), offset)
    }

    fn read8(&self, at: usize) -> u8 {
        let (mmio, base) = self.mmio(self.common);
        mmio.read8(base + at)
    }

    fn write8(&self, at: usize, value: u8) {
        let (mmio, base) = self.mmio(self.common);
        mmio.write8(base + at, value)
    }

    fn read16(&self, at: usize) -> u16 {
        let (mmio, base) = self.mmio(self.common);
        mmio.read16(base + at)
    }

    fn write16(&self, at: usize, value: u16) {
        let (mmio, base) = self.mmio(self.common);
        mmio.write16(base + at, value)
    }

    fn read32(&self, at: usize) -> u32 {
        let (mmio, base) = self.mmio(self.common);
        mmio.read32(base + at)
    }

    fn write32(&self, at: usize, value: u32) {
        let (mmio, base) = self.mmio(self.common);
        mmio.write32(base + at, value)
    }

    /// A 64-bit field, written as two halves, low first, as 4.1.3.1 allows.
    fn write64(&self, at: usize, value: u64) {
        self.write32(at, value as u32);
        self.write32(at + 4, (value >> 32) as u32);
    }

    /// Read or write one sector through the sector page, and wait for the
    /// device to finish it.
    fn transfer(&mut self, kind: u32, sector: u64) -> Result<(), Error> {
        blk::header(self.pages.bytes(QUEUE_HEADER, 0, blk::HEADER_LEN), kind, sector);
        // Anything but OK, so a device that never writes it is an error.
        self.pages.bytes(QUEUE_HEADER, STATUS_AT, 1)[0] = 0xff;
        let header = self.pages.physical(QUEUE_HEADER)?;
        let data = self.pages.physical(QUEUE_SECTOR)?;
        let chain = [
            Buffer {
                address: header,
                len: blk::HEADER_LEN as u32,
                device_writes: false,
            },
            Buffer {
                address: data,
                len: blk::SECTOR as u32,
                device_writes: kind == blk::T_IN,
            },
            Buffer {
                address: header + STATUS_AT as u64,
                len: 1,
                device_writes: true,
            },
        ];
        let token = self.queue.submit(&chain).ok_or(Error::NoSpace)?;
        let (mmio, base) = self.mmio(self.doorbell);
        mmio.write16(base, REQUEST_QUEUE);
        loop {
            match self.queue.take() {
                Some(used) if used.token == token => break,
                Some(_) => {}
                None => corrode_rt::yield_now(),
            }
        }
        match self.pages.bytes(QUEUE_HEADER, STATUS_AT, 1)[0] {
            blk::S_OK => Ok(()),
            blk::S_UNSUPP => Err(Error::Refused),
            _ => Err(Error::Invalid),
        }
    }

    fn sector(&mut self) -> &mut [u8] {
        self.pages.bytes(QUEUE_SECTOR, 0, blk::SECTOR)
    }
}

/// Bring the device up, as 3.1.1 lists it, as far as a running request
/// queue.
fn bring_up() -> Result<Disk, Error> {
    let config = Config(store::DEVICE);
    config.enable()?;
    let layout = Layout::read(&config);
    let (Some(common_region), Some(notify_region)) = (layout.common, layout.notify) else {
        println!("no virtio common or notify structure");
        return Err(Error::Invalid);
    };

    let mut bars: [Option<Mmio>; 6] = [None, None, None, None, None, None];
    for region in [common_region, notify_region] {
        let bar = region.bar as usize;
        if bars[bar].is_none() {
            let slot = store::BARS + bar;
            if identify(slot).map(|(kind, _)| kind) != Ok(abi::kind::DEVICE_MEMORY) {
                println!("BAR {} holds a structure and was not given to us", bar);
                return Err(Error::Empty);
            }
            bars[bar] = Some(Mmio::map(slot)?);
        }
    }

    let pages = Pages::map(store::QUEUE)?;
    let ring = queue::Layout::new(queue::MAX_SIZE).ok_or(Error::Invalid)?;
    // SAFETY: page 0 of the queue memory is ours, mapped writable for the
    // life of the task, 4 KiB aligned, and holds the whole ring; the only
    // other party to it is the device it is about to be given to.
    let queue = unsafe { Queue::new(pages.page(QUEUE_RING), ring) };
    let mut disk = Disk {
        bars,
        common: (common_region.bar as usize, common_region.offset as usize),
        doorbell: (notify_region.bar as usize, notify_region.offset as usize),
        queue,
        pages,
    };

    disk.write8(common::DEVICE_STATUS, 0);
    while disk.read8(common::DEVICE_STATUS) != 0 {
        corrode_rt::yield_now();
    }
    let mut state = status::ACKNOWLEDGE | status::DRIVER;
    disk.write8(common::DEVICE_STATUS, status::ACKNOWLEDGE);
    disk.write8(common::DEVICE_STATUS, state);

    // Version 1 and nothing else: no read-only flag to honour, no
    // multiple queues, no discard.
    disk.write32(common::DEVICE_FEATURE_SELECT, 1);
    let offered = (disk.read32(common::DEVICE_FEATURE) as u64) << 32;
    if offered & F_VERSION_1 == 0 {
        println!("the device does not speak virtio 1");
        disk.write8(common::DEVICE_STATUS, state | status::FAILED);
        return Err(Error::Invalid);
    }
    disk.write32(common::DRIVER_FEATURE_SELECT, 0);
    disk.write32(common::DRIVER_FEATURE, 0);
    disk.write32(common::DRIVER_FEATURE_SELECT, 1);
    disk.write32(common::DRIVER_FEATURE, (F_VERSION_1 >> 32) as u32);
    state |= status::FEATURES_OK;
    disk.write8(common::DEVICE_STATUS, state);
    if disk.read8(common::DEVICE_STATUS) & status::FEATURES_OK == 0 {
        println!("the device would not accept our features");
        return Err(Error::Refused);
    }

    disk.write16(common::MSIX_CONFIG, common::NO_VECTOR);
    disk.write16(common::QUEUE_SELECT, REQUEST_QUEUE);
    let offered = disk.read16(common::QUEUE_SIZE);
    if offered < queue::MAX_SIZE {
        println!("the request queue only has {} entries", offered);
        return Err(Error::NoSpace);
    }
    disk.write16(common::QUEUE_SIZE, queue::MAX_SIZE);
    disk.write16(common::QUEUE_MSIX_VECTOR, common::NO_VECTOR);
    let ring_physical = disk.pages.physical(QUEUE_RING)?;
    disk.write64(common::QUEUE_DESC, ring_physical + ring.desc as u64);
    disk.write64(common::QUEUE_DRIVER, ring_physical + ring.avail as u64);
    disk.write64(common::QUEUE_DEVICE, ring_physical + ring.used as u64);
    let notify_offset = disk.read16(common::QUEUE_NOTIFY_OFF) as usize;
    disk.doorbell.1 += notify_offset * layout.notify_multiplier as usize;
    disk.write16(common::QUEUE_ENABLE, 1);

    disk.write8(common::DEVICE_STATUS, state | status::DRIVER_OK);
    Ok(disk)
}

fn serve(disk: &mut Disk, message: Message) -> Result<[u64; 4], Error> {
    match message.label {
        store::LOAD => {
            disk.transfer(blk::T_IN, THEME_SECTOR)?;
            let theme = Theme::from_record(disk.sector()).ok_or(Error::Empty)?;
            Ok(theme.to_words())
        }
        store::SAVE => {
            let record = Theme::from_words(message.words).to_record();
            let sector = disk.sector();
            sector.fill(0);
            sector[..RECORD_LEN].copy_from_slice(&record);
            disk.transfer(blk::T_OUT, THEME_SECTOR)?;
            Ok([0; 4])
        }
        store::READ => {
            disk.transfer(blk::T_IN, message.words[0])?;
            Ok([0; 4])
        }
        // The theme's sector only through SAVE, which checks the record.
        store::WRITE if message.words[0] == THEME_SECTOR => Err(Error::Invalid),
        store::WRITE => {
            disk.transfer(blk::T_OUT, message.words[0])?;
            Ok([0; 4])
        }
        _ => Err(Error::Invalid),
    }
}

fn main() {
    corrode_rt::log::set(store::LOG);
    // Without a disk this still answers, every call with why: the desktop
    // calls and waits, and a driver that had gone would keep it waiting.
    let mut disk = match bring_up() {
        Ok(disk) => {
            println!("ready");
            Some(disk)
        }
        Err(error) => {
            println!("no disk: {:?}", error);
            None
        }
    };

    loop {
        let received = match recv(store::SERVE, None, Some(store::REPLY)) {
            Ok(received) => received,
            Err(error) => {
                println!("cannot receive: {:?}", error);
                return;
            }
        };
        let answer = match &mut disk {
            Some(disk) => serve(disk, received.message),
            None => Err(Error::Empty),
        };
        let (label, words) = match answer {
            Ok(words) => (abi::STATUS_OK, words),
            Err(error) => (error.code(), [0; 4]),
        };
        if identify(store::REPLY).is_ok() {
            let _ = reply(store::REPLY, Message::new(label, words));
        }
    }
}
