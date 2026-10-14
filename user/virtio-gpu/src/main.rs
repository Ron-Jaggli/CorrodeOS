//! A display driver for the virtio GPU, in ring 3.
//!
//! The kernel's compositor draws through the `Renderer` trait. When this
//! driver is running, what is behind that trait is a call to this task: it
//! receives command records in memory it shares with the kernel, draws them
//! with the same software renderer the kernel would have used -- into
//! memory its device scans out -- and when told to present, has the device
//! copy the changed rectangles to the host and show them.
//!
//! Everything a real GPU driver needs from the system is used here: the
//! device's configuration space, its registers, an MSI-X interrupt, memory
//! the device reads by DMA, and the physical addresses of that memory. The
//! capabilities for all of them are listed in `corrode_abi::gpu`, and that
//! list is everything this task can touch.
//!
//! The device protocol is the Virtual I/O Device specification 1.2: 3.1 for
//! bringing a device up, 4.1 for the PCI transport, 5.7 for the GPU.

#![no_std]
#![no_main]

use corrode_render::damage::Damage;
use corrode_render::software::SoftwareRenderer;
use corrode_render::{wire, Command, CommandList, PixelFormat, Rect, Renderer, SurfaceInfo};
use corrode_rt::abi::gpu;
use corrode_rt::device::{Config, Interrupt, Mmio};
use corrode_rt::{abi, identify, println, recv, reply, Error, Message};
use corrode_virtio::gpu as cmd;
use corrode_virtio::pci::{common, Layout};
use corrode_virtio::queue::{self, Buffer, Queue};
use corrode_virtio::{status, F_VERSION_1};

corrode_rt::entry!(main);

/// The one resource: the screen.
const RESOURCE: u32 = 1;
/// The control queue is queue 0.
const CONTROL_QUEUE: u16 = 0;
const PAGE: usize = 4096;

/// How the queue memory is laid out, by page.
const QUEUE_RING: usize = 0;
const QUEUE_REQUESTS: usize = 1;
/// Two pages of responses, so a batch can be as long as the damage list.
const QUEUE_RESPONSES: usize = 2;
const QUEUE_ENTRIES: usize = 4;

const REQUEST_SLOT: usize = 256;
const RESPONSE_SLOT: usize = 512;
/// Requests in one batch: eight rectangles to transfer and one flush.
const BATCH: usize = 9;

/// Most command records one submit may carry.
const MAX_COMMANDS: usize = 512;

/// A structure inside one of the device's BARs.
#[derive(Clone, Copy)]
struct Window {
    bar: usize,
    offset: usize,
}

/// The device, set up.
struct Device {
    bars: [Option<Mmio>; 6],
    common: Window,
    doorbell: Window,
    queue: Queue,
    /// Virtual and physical addresses of the queue memory's pages.
    pages: Pages,
    /// The interrupt, if the device took a vector. Without one the driver
    /// polls, which works and costs a CPU.
    irq: Option<Interrupt>,
}

/// A memory capability, mapped, with every page's physical address.
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
        // SAFETY: inside one page of the mapping, which is ours for the life
        // of the task. The device reads requests and writes responses here,
        // but only between `submit` and `take`, while this slice is not held.
        unsafe { core::slice::from_raw_parts_mut(self.page(page).add(offset), len) }
    }
}

impl Device {
    fn regs(&self, window: Window) -> (&Mmio, usize) {
        (
            self.bars[window.bar].as_ref().expect("mapped in bring_up"),
            window.offset,
        )
    }

    fn read8(&self, at: usize) -> u8 {
        let (mmio, base) = self.regs(self.common);
        mmio.read8(base + at)
    }

    fn write8(&self, at: usize, value: u8) {
        let (mmio, base) = self.regs(self.common);
        mmio.write8(base + at, value)
    }

    fn write16(&self, at: usize, value: u16) {
        let (mmio, base) = self.regs(self.common);
        mmio.write16(base + at, value)
    }

    fn read16(&self, at: usize) -> u16 {
        let (mmio, base) = self.regs(self.common);
        mmio.read16(base + at)
    }

    fn write32(&self, at: usize, value: u32) {
        let (mmio, base) = self.regs(self.common);
        mmio.write32(base + at, value)
    }

    fn read32(&self, at: usize) -> u32 {
        let (mmio, base) = self.regs(self.common);
        mmio.read32(base + at)
    }

    /// A 64-bit field, written as two halves, low first, as 4.1.3.1 allows.
    fn write64(&self, at: usize, value: u64) {
        self.write32(at, value as u32);
        self.write32(at + 4, (value >> 32) as u32);
    }

    fn ring(&self) {
        let (mmio, base) = self.regs(self.doorbell);
        mmio.write16(base, CONTROL_QUEUE);
    }

    /// Send a batch of requests and wait until the device has answered all
    /// of them. Each is `(request length, extra buffers, response length)`;
    /// request `i` was written at slot `i` of the request page.
    ///
    /// Returns the response type of the first answer that was not success,
    /// or the last one if all were.
    fn run(&mut self, batch: &[(usize, &[Buffer], usize)]) -> Result<u32, Error> {
        let requests = self.pages.physical(QUEUE_REQUESTS)?;
        let responses = self.pages.physical(QUEUE_RESPONSES)?;
        let responses_two = self.pages.physical(QUEUE_RESPONSES + 1)?;
        let mut tokens = [u16::MAX; BATCH];

        for (i, (len, extra, response_len)) in batch.iter().enumerate() {
            let response = if i < PAGE / RESPONSE_SLOT {
                responses + (i * RESPONSE_SLOT) as u64
            } else {
                responses_two + ((i - PAGE / RESPONSE_SLOT) * RESPONSE_SLOT) as u64
            };
            let mut chain = [Buffer {
                address: 0,
                len: 0,
                device_writes: false,
            }; queue::MAX_SIZE as usize];
            chain[0] = Buffer {
                address: requests + (i * REQUEST_SLOT) as u64,
                len: *len as u32,
                device_writes: false,
            };
            chain[1..1 + extra.len()].copy_from_slice(extra);
            chain[1 + extra.len()] = Buffer {
                address: response,
                len: *response_len as u32,
                device_writes: true,
            };
            tokens[i] = self
                .queue
                .submit(&chain[..2 + extra.len()])
                .ok_or(Error::NoSpace)?;
        }
        self.ring();

        let mut answered = 0;
        while answered < batch.len() {
            while let Some(used) = self.queue.take() {
                if tokens.contains(&used.token) {
                    answered += 1;
                }
            }
            if answered < batch.len() {
                match self.irq {
                    // Asleep in the kernel until the device signals.
                    Some(irq) => {
                        irq.wait()?;
                    }
                    None => corrode_rt::yield_now(),
                }
            }
        }

        let mut worst = cmd::RESP_OK_NODATA;
        for i in 0..batch.len() {
            let (page, offset) = if i < PAGE / RESPONSE_SLOT {
                (QUEUE_RESPONSES, i * RESPONSE_SLOT)
            } else {
                (
                    QUEUE_RESPONSES + 1,
                    (i - PAGE / RESPONSE_SLOT) * RESPONSE_SLOT,
                )
            };
            let kind = cmd::response_type(self.pages.bytes(page, offset, cmd::HEADER_LEN));
            worst = kind;
            if kind >= cmd::RESP_ERR_UNSPEC {
                return Ok(kind);
            }
        }
        Ok(worst)
    }

    /// One request, its answer checked.
    fn one(&mut self, len: usize, extra: &[Buffer], response_len: usize) -> Result<(), Error> {
        match self.run(&[(len, extra, response_len)])? {
            kind if kind >= cmd::RESP_ERR_UNSPEC => {
                println!("the device refused a command: {:#x}", kind);
                Err(Error::Refused)
            }
            _ => Ok(()),
        }
    }

    fn request(&mut self, slot: usize) -> &mut [u8] {
        self.pages
            .bytes(QUEUE_REQUESTS, slot * REQUEST_SLOT, REQUEST_SLOT)
    }
}

/// Bring the device up, as 3.1.1 lists it, as far as a running control
/// queue.
fn bring_up() -> Result<Device, Error> {
    let config = Config(gpu::DEVICE);
    config.enable()?;
    let layout = Layout::read(&config);
    let (Some(common_region), Some(notify_region)) = (layout.common, layout.notify) else {
        println!("no virtio common or notify structure");
        return Err(Error::Invalid);
    };

    // Map the BARs the structures are in, and only those.
    let mut bars: [Option<Mmio>; 6] = [None, None, None, None, None, None];
    for region in [common_region, notify_region] {
        let bar = region.bar as usize;
        if bars[bar].is_none() {
            let slot = gpu::BARS + bar;
            if identify(slot).map(|(kind, _)| kind) != Ok(abi::kind::DEVICE_MEMORY) {
                println!("BAR {} holds a structure and was not given to us", bar);
                return Err(Error::Empty);
            }
            bars[bar] = Some(Mmio::map(slot)?);
        }
    }

    let pages = Pages::map(gpu::QUEUE)?;
    let ring = queue::Layout::new(queue::MAX_SIZE).ok_or(Error::Invalid)?;
    // SAFETY: page 0 of the queue memory is ours, mapped writable for the
    // life of the task, 4 KiB aligned, and holds the whole ring; the only
    // other party to it is the device it is about to be given to.
    let queue = unsafe { Queue::new(pages.page(QUEUE_RING), ring) };

    let mut device = Device {
        bars,
        common: Window {
            bar: common_region.bar as usize,
            offset: common_region.offset as usize,
        },
        doorbell: Window {
            bar: notify_region.bar as usize,
            offset: notify_region.offset as usize,
        },
        queue,
        pages,
        irq: None,
    };

    // Reset, and wait for the device to say it has.
    device.write8(common::DEVICE_STATUS, 0);
    while device.read8(common::DEVICE_STATUS) != 0 {
        corrode_rt::yield_now();
    }
    let mut state = status::ACKNOWLEDGE;
    device.write8(common::DEVICE_STATUS, state);
    state |= status::DRIVER;
    device.write8(common::DEVICE_STATUS, state);

    // Features: version 1, and nothing else. No 3D, no EDID, no blobs.
    device.write32(common::DEVICE_FEATURE_SELECT, 1);
    let offered = (device.read32(common::DEVICE_FEATURE) as u64) << 32;
    if offered & F_VERSION_1 == 0 {
        println!("the device does not speak virtio 1");
        device.write8(common::DEVICE_STATUS, state | status::FAILED);
        return Err(Error::Invalid);
    }
    device.write32(common::DRIVER_FEATURE_SELECT, 0);
    device.write32(common::DRIVER_FEATURE, 0);
    device.write32(common::DRIVER_FEATURE_SELECT, 1);
    device.write32(common::DRIVER_FEATURE, (F_VERSION_1 >> 32) as u32);
    state |= status::FEATURES_OK;
    device.write8(common::DEVICE_STATUS, state);
    if device.read8(common::DEVICE_STATUS) & status::FEATURES_OK == 0 {
        println!("the device would not accept our features");
        return Err(Error::Refused);
    }

    // Configuration changes are not interesting yet.
    device.write16(common::MSIX_CONFIG, common::NO_VECTOR);

    device.write16(common::QUEUE_SELECT, CONTROL_QUEUE);
    let offered = device.read16(common::QUEUE_SIZE);
    if offered < queue::MAX_SIZE {
        println!("the control queue only has {} entries", offered);
        return Err(Error::NoSpace);
    }
    device.write16(common::QUEUE_SIZE, queue::MAX_SIZE);
    let ring_physical = device.pages.physical(QUEUE_RING)?;
    device.write64(common::QUEUE_DESC, ring_physical + ring.desc as u64);
    device.write64(common::QUEUE_DRIVER, ring_physical + ring.avail as u64);
    device.write64(common::QUEUE_DEVICE, ring_physical + ring.used as u64);

    // MSI-X entry 0, which is the one the kernel aimed at our CPU. The
    // device reads back NO_VECTOR if it could not take it.
    device.write16(common::QUEUE_MSIX_VECTOR, 0);
    if device.read16(common::QUEUE_MSIX_VECTOR) == 0
        && identify(gpu::IRQ).map(|(kind, _)| kind) == Ok(abi::kind::INTERRUPT)
    {
        device.irq = Some(Interrupt(gpu::IRQ));
    }

    let notify_offset = device.read16(common::QUEUE_NOTIFY_OFF) as usize;
    device.doorbell.offset += notify_offset * layout.notify_multiplier as usize;
    device.write16(common::QUEUE_ENABLE, 1);

    device.write8(common::DEVICE_STATUS, state | status::DRIVER_OK);
    Ok(device)
}

/// The screen: a resource the size of the display, backed by our memory,
/// shown on scanout 0.
struct Screen {
    device: Device,
    renderer: SoftwareRenderer,
    info: SurfaceInfo,
}

fn open_screen(mut device: Device) -> Result<(Screen, u64), Error> {
    // What is plugged in?
    let len = cmd::get_display_info(device.request(0));
    device.one(len, &[], cmd::DISPLAY_INFO_LEN)?;
    let answer = device
        .pages
        .bytes(QUEUE_RESPONSES, 0, cmd::DISPLAY_INFO_LEN);
    let mut chosen = None;
    let mut displays = 0;
    for index in 0..cmd::MAX_SCANOUTS {
        if let Some(display) = cmd::display(answer, index) {
            if display.enabled {
                displays += 1;
                chosen = chosen.or(Some(display.rect));
            }
        }
    }
    let Some(mode) = chosen.filter(|mode| mode.width > 0 && mode.height > 0) else {
        println!("no display is enabled");
        return Err(Error::Invalid);
    };

    let screen = Pages::map(gpu::SCREEN)?;
    let pitch = mode.width as usize * 4;
    let bytes = pitch * mode.height as usize;
    if bytes > screen.len {
        println!(
            "{}x{} needs {} KiB, and there are {}",
            mode.width,
            mode.height,
            bytes / 1024,
            screen.len / 1024
        );
        return Err(Error::NoSpace);
    }

    let len = cmd::resource_create_2d(
        device.request(0),
        RESOURCE,
        cmd::FORMAT_B8G8R8X8_UNORM,
        mode.width,
        mode.height,
    );
    device.one(len, &[], cmd::HEADER_LEN)?;

    // Backing, page by page: the memory is not contiguous and need not be.
    // Runs of pages that happen to be adjacent are merged, which in practice
    // makes the list short.
    let pages = bytes.div_ceil(PAGE);
    let mut entries = 0usize;
    let mut run: Option<(u64, u32)> = None;
    let entry_capacity = (gpu::QUEUE_PAGES - QUEUE_ENTRIES) * PAGE / cmd::ENTRY_LEN;
    for page in 0..=pages {
        let next = if page < pages {
            Some(screen.physical(page)?)
        } else {
            None
        };
        match (run, next) {
            (Some((start, len)), Some(address)) if start + len as u64 == address => {
                run = Some((start, len + PAGE as u32));
            }
            (previous, next) => {
                if let Some((start, len)) = previous {
                    if entries == entry_capacity {
                        return Err(Error::NoSpace);
                    }
                    let (page, offset) = (
                        QUEUE_ENTRIES + entries * cmd::ENTRY_LEN / PAGE,
                        entries * cmd::ENTRY_LEN % PAGE,
                    );
                    cmd::backing_entry(
                        device.pages.bytes(page, offset, cmd::ENTRY_LEN),
                        start,
                        len,
                    );
                    entries += 1;
                }
                run = next.map(|address| (address, PAGE as u32));
            }
        }
    }
    let mut extra = [Buffer {
        address: 0,
        len: 0,
        device_writes: false,
    }; gpu::QUEUE_PAGES];
    let entry_pages = (entries * cmd::ENTRY_LEN).div_ceil(PAGE);
    for (i, buffer) in extra.iter_mut().enumerate().take(entry_pages) {
        let len = (entries * cmd::ENTRY_LEN - i * PAGE).min(PAGE);
        *buffer = Buffer {
            address: device.pages.physical(QUEUE_ENTRIES + i)?,
            len: len as u32,
            device_writes: false,
        };
    }
    let len = cmd::resource_attach_backing(device.request(0), RESOURCE, entries as u32);
    device.one(len, &extra[..entry_pages], cmd::HEADER_LEN)?;

    let whole = cmd::Rect {
        x: 0,
        y: 0,
        width: mode.width,
        height: mode.height,
    };
    let len = cmd::set_scanout(device.request(0), 0, RESOURCE, whole);
    device.one(len, &[], cmd::HEADER_LEN)?;

    let info = SurfaceInfo {
        width: mode.width,
        height: mode.height,
        pitch: pitch as u32,
        format: PixelFormat::XRGB8888,
    };
    // Drawn straight into the backing, with no back buffer. Nothing the
    // device shows changes until a transfer copies it, so the backing is
    // already what a back buffer would be.
    //
    // SAFETY: `screen` is `bytes` or more of memory mapped writable for the
    // life of this task, and nothing else in it is ever pointed at it.
    let mut renderer =
        unsafe { SoftwareRenderer::new(screen.page(0), info) }.map_err(|_| Error::Invalid)?;

    let atlas = Pages::map(gpu::ATLAS)?;
    let wanted = (gpu::ATLAS_WIDTH * gpu::ATLAS_HEIGHT) as usize;
    if atlas.len >= wanted {
        // SAFETY: mapped writable for the life of the task, and this is the
        // only reference to it that will ever exist.
        let data = unsafe { core::slice::from_raw_parts_mut(atlas.base, wanted) };
        renderer.attach_mask_atlas(data, gpu::ATLAS_WIDTH);
    }

    Ok((
        Screen {
            device,
            renderer,
            info,
        },
        displays,
    ))
}

impl Screen {
    /// Copy the damaged rectangles to the host and show them: one transfer
    /// each and one flush, as one batch, and one wait.
    fn present(&mut self, rects: &[Rect]) -> Result<(), Error> {
        let bounds = Rect::new(0, 0, self.info.width, self.info.height);
        let mut batch: [(usize, &[Buffer], usize); BATCH] = [(0, &[], 0); BATCH];
        let mut count = 0;
        let mut union = Rect::new(0, 0, 0, 0);
        for rect in rects.iter().take(BATCH - 1) {
            let Some(rect) = rect.intersect(&bounds) else {
                continue;
            };
            union = union.union(&rect);
            let device_rect = cmd::Rect {
                x: rect.x as u32,
                y: rect.y as u32,
                width: rect.width,
                height: rect.height,
            };
            let offset = rect.y as u64 * self.info.pitch as u64 + rect.x as u64 * 4;
            let len =
                cmd::transfer_to_host_2d(self.device.request(count), RESOURCE, device_rect, offset);
            batch[count] = (len, &[], cmd::HEADER_LEN);
            count += 1;
        }
        if count == 0 {
            return Ok(());
        }
        let len = cmd::resource_flush(
            self.device.request(count),
            RESOURCE,
            cmd::Rect {
                x: union.x as u32,
                y: union.y as u32,
                width: union.width,
                height: union.height,
            },
        );
        batch[count] = (len, &[], cmd::HEADER_LEN);
        count += 1;

        match self.device.run(&batch[..count])? {
            kind if kind >= cmd::RESP_ERR_UNSPEC => Err(Error::Refused),
            _ => Ok(()),
        }
    }
}

/// The kernel's request, done. Returns the words of the answer.
fn serve(
    screen: &mut Option<Screen>,
    shared: &Pages,
    surfaces: &mut usize,
    message: Message,
) -> Result<[u64; 4], Error> {
    let words = message.words;
    if message.label == gpu::INIT {
        if let Some(screen) = screen {
            return Ok([
                screen.info.width as u64,
                screen.info.height as u64,
                screen.info.pitch as u64,
                0,
            ]);
        }
        let (opened, displays) = open_screen(bring_up()?)?;
        let info = opened.info;
        println!(
            "{}x{} on {} display(s), {} the device's interrupt",
            info.width,
            info.height,
            displays,
            if opened.device.irq.is_some() {
                "woken by"
            } else {
                "polling, without"
            }
        );
        *screen = Some(opened);
        return Ok([
            info.width as u64,
            info.height as u64,
            info.pitch as u64,
            displays,
        ]);
    }

    let screen = screen.as_mut().ok_or(Error::Stale)?;
    // SAFETY: the shared memory is mapped for the life of the task. The
    // kernel writes it only while it is not blocked in a call to us, and we
    // read it only while it is: this is that window.
    let bytes = unsafe { core::slice::from_raw_parts(shared.base, shared.len) };

    match message.label {
        gpu::SURFACE => {
            let slot = gpu::SURFACES + *surfaces;
            let mapped = corrode_rt::map_surface(slot)?;
            if mapped.pitch != mapped.width * 4 {
                return Err(Error::Invalid);
            }
            // SAFETY: the kernel mapped `width * height` pixels at that
            // address, readable, for as long as we hold the capability, and
            // we never drop it.
            unsafe {
                screen.renderer.attach_surface(
                    words[0] as u32,
                    mapped.address,
                    mapped.width,
                    mapped.height,
                )
            }
            .map_err(|_| Error::Invalid)?;
            *surfaces += 1;
            Ok([0; 4])
        }

        gpu::SUBMIT => {
            let count = words[0] as usize;
            if count > MAX_COMMANDS || count * wire::RECORD > bytes.len() {
                return Err(Error::Invalid);
            }
            let mut storage = [Command::Clear {
                color: corrode_render::Color::BLACK,
            }; MAX_COMMANDS];
            let mut list = CommandList::new(&mut storage);
            for record in bytes[..count * wire::RECORD]
                .as_chunks::<{ wire::RECORD }>()
                .0
            {
                let command = wire::decode(record).ok_or(Error::Invalid)?;
                list.push(command);
            }
            screen.renderer.submit(&list).map_err(|_| Error::Invalid)?;
            Ok([0; 4])
        }

        gpu::UPLOAD => {
            let at = Rect::new(
                words[0] as i32,
                words[1] as i32,
                words[2] as u32,
                words[3] as u32,
            );
            let len = at.area() as usize;
            if len > bytes.len() {
                return Err(Error::Invalid);
            }
            screen
                .renderer
                .upload_mask(at, &bytes[..len])
                .map_err(|_| Error::Invalid)?;
            Ok([0; 4])
        }

        gpu::PRESENT => {
            let count = (words[0] as usize).min(corrode_render::damage::CAPACITY);
            let mut rects = [Rect::new(0, 0, 0, 0); corrode_render::damage::CAPACITY];
            for (rect, raw) in rects
                .iter_mut()
                .zip(bytes.as_chunks::<{ wire::RECT }>().0)
                .take(count)
            {
                *rect = wire::get_rect(raw);
            }
            // Without a back buffer this copies nothing, and says so by
            // succeeding; it is called because a renderer is presented.
            let mut damage = Damage::new();
            for rect in &rects[..count] {
                damage.add(*rect);
            }
            screen
                .renderer
                .present(&damage)
                .map_err(|_| Error::Invalid)?;
            screen.present(&rects[..count])?;
            Ok([0; 4])
        }

        _ => Err(Error::Invalid),
    }
}

fn main() {
    corrode_rt::log::set(gpu::LOG);

    let shared = match Pages::map(gpu::SHARED) {
        Ok(shared) => shared,
        Err(error) => {
            println!("no shared memory: {:?}", error);
            return;
        }
    };
    let mut screen = None;
    let mut surfaces = 0;

    loop {
        let into = gpu::SURFACES + surfaces;
        let received = match recv(gpu::SERVE, Some(into), Some(gpu::REPLY)) {
            Ok(received) => received,
            Err(error) => {
                println!("cannot receive: {:?}", error);
                return;
            }
        };
        let (label, words) = match serve(&mut screen, &shared, &mut surfaces, received.message) {
            Ok(words) => (abi::STATUS_OK, words),
            Err(error) => (error.code(), [0; 4]),
        };
        // A call that came as a one-way send has nobody to answer.
        if identify(gpu::REPLY).is_ok() {
            let _ = reply(gpu::REPLY, Message::new(label, words));
        }
    }
}
