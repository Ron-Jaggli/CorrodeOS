//! Where the desktop keeps its theme between boots: a virtio block device,
//! driven by a ring 3 task, `virtio-blk`, that holds its device, its queue
//! memory and an endpoint, and nothing else. See `corrode_abi::store`.
//!
//! No disk is the usual case, and fine: the theme is then whatever
//! /etc/desktop.conf says, every boot.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use corrode_abi::store;
use corrode_theme::Theme;

use crate::cap::{self, Rights};
use crate::{ipc, memory, modules, pci, println, task};

/// Slots in the kernel's capability space.
const STORE_ENDPOINT: cap::CapPtr = 82;
const STORE_DEVICE: cap::CapPtr = 83;
const STORE_QUEUE: cap::CapPtr = 84;
/// BAR `n` at `STORE_BARS + n`: 86 to 91.
const STORE_BARS: cap::CapPtr = 86;

static RUNNING: AtomicBool = AtomicBool::new(false);
/// The queue memory's object, for `read_sector`.
static QUEUE: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Its page the driver reads a sector into.
const SECTOR_PAGE: usize = 2;

/// Find a virtio disk and start its driver. Says whether there is one.
pub fn start() -> bool {
    let Some((index, found)) = pci::find(|device| {
        device.header.vendor == corrode_abi::gpu::VIRTIO_VENDOR
            && matches!(
                device.header.device,
                store::VIRTIO_BLK | store::VIRTIO_BLK_TRANSITIONAL
            )
    }) else {
        println!("store      : no disk -- settings last until shutdown");
        return false;
    };
    let Some(driver) = modules::find("virtio-blk")
        .and_then(|module| task::spawn_program_on("virtio-blk", module.bytes, crate::cpu::index()).ok())
    else {
        println!("store      : a disk at {}, but no driver for it", found.address);
        return false;
    };
    // Kernel-visible, so `read_sector` can take what the driver read.
    let Some(queue) = memory::create(store::QUEUE_PAGES, true) else {
        println!("store      : no memory for the disk's queue");
        return false;
    };

    let mut given = cap::mint(
        0,
        STORE_ENDPOINT,
        cap::Object::Endpoint(ipc::Endpoint::new()),
        Rights::ALL,
    )
    .is_ok()
        && cap::mint(0, STORE_DEVICE, cap::Object::PciDevice(index), Rights::ALL).is_ok()
        && cap::mint(0, STORE_QUEUE, cap::Object::Memory(queue), Rights::ALL).is_ok()
        && cap::grant(driver, STORE_ENDPOINT, store::SERVE, Rights::READ).is_ok()
        && cap::grant(driver, crate::LOG_ENDPOINT, store::LOG, Rights::WRITE).is_ok()
        && cap::grant(driver, STORE_DEVICE, store::DEVICE, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, STORE_QUEUE, store::QUEUE, Rights::READ_WRITE).is_ok();
    let mut bars = 0;
    for bar in 0..6u8 {
        if let Some(region) = pci::region_of(index, bar) {
            let slot = STORE_BARS + bar as usize;
            given &= cap::mint(0, slot, cap::Object::DeviceMemory(region), Rights::ALL).is_ok()
                && cap::grant(driver, slot, store::BARS + bar as usize, Rights::READ_WRITE).is_ok();
            bars += 1;
        }
    }
    crate::check(
        "store",
        "the disk's driver holds its device, its memory, and nothing else",
        given && cap::slots_used(driver) == Some(4 + bars),
    );
    if !given {
        return false;
    }
    QUEUE.store(queue, Ordering::Release);
    // The window server answers Settings' passcode change, which seals the
    // vault again on the disk: it may send to the driver too, from the same
    // slot, and do nothing else with it.
    let server = crate::WINDOW_SERVER.load(Ordering::Acquire);
    if server != usize::MAX && cap::grant(server, STORE_ENDPOINT, STORE_ENDPOINT, Rights::WRITE).is_err() {
        println!("store      : the window server cannot reach the disk; the passcode cannot be changed");
    }
    crate::start_all(&[Some(driver)]);
    println!("store      : a disk at {}, driven in ring 3", found.address);
    RUNNING.store(true, Ordering::Release);
    true
}

/// One call to the driver at a time: the painter saves the theme and the
/// desktop's loop reads and writes the vault, and both go through the one
/// sector page. Held across the call, so waited for by yielding, not
/// spinning with interrupts off.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Run `work` with the driver to itself.
fn alone<T>(work: impl FnOnce() -> T) -> T {
    while BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        task::yield_now();
    }
    let result = work();
    BUSY.store(false, Ordering::Release);
    result
}

fn call(label: u64, words: [u64; 4]) -> Option<[u64; 4]> {
    if !RUNNING.load(Ordering::Acquire) {
        return None;
    }
    alone(|| raw_call(label, words))
}

/// A call, for one that already holds the driver.
fn raw_call(label: u64, words: [u64; 4]) -> Option<[u64; 4]> {
    let answer = ipc::call(STORE_ENDPOINT, ipc::Message { label, words }, None, None).ok()?;
    (answer.message.label == corrode_abi::STATUS_OK).then_some(answer.message.words)
}

/// The sector page of the queue memory, as the kernel sees it.
fn sector_page() -> Option<*mut u8> {
    let (address, len) = memory::in_kernel(QUEUE.load(Ordering::Acquire))?;
    // SAFETY: the page is inside the object, checked against its length.
    (len >= (SECTOR_PAGE + 1) * 4096).then(|| unsafe { address.add(SECTOR_PAGE * 4096) })
}

/// The theme kept on the disk, if there is a disk and it keeps one.
pub fn load() -> Option<Theme> {
    let words = call(store::LOAD, [0; 4]);
    match words {
        Some(words) => {
            let theme = Theme::from_words(words);
            let tone = if theme.dark { "dark" } else { "light" };
            println!("store      : read the theme kept on the disk ({tone})");
            Some(theme)
        }
        None if RUNNING.load(Ordering::Acquire) => {
            println!("store      : the disk keeps no theme yet");
            None
        }
        None => None,
    }
}

/// One sector of the disk, if there is a disk and it gives it.
pub fn read_sector(sector: u64) -> Option<[u8; 512]> {
    if !RUNNING.load(Ordering::Acquire) {
        return None;
    }
    alone(|| {
        let page = sector_page()?;
        raw_call(store::READ, [sector, 0, 0, 0])?;
        let mut bytes = [0u8; 512];
        // SAFETY: the sector page, which the kernel can see. The driver
        // wrote the sector there before answering and touches it only
        // during a call, and this one holds the driver.
        unsafe { core::ptr::copy_nonoverlapping(page, bytes.as_mut_ptr(), bytes.len()) };
        Some(bytes)
    })
}

/// Write one sector of the disk. Not the theme's: that is `save`'s.
pub fn write_sector(sector: u64, bytes: &[u8; 512]) -> bool {
    if !RUNNING.load(Ordering::Acquire) {
        return false;
    }
    alone(|| {
        let Some(page) = sector_page() else {
            return false;
        };
        // SAFETY: as in `read_sector`; the driver reads the page only
        // during the call below.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), page, bytes.len()) };
        let answer = ipc::call(STORE_ENDPOINT, ipc::Message { label: store::WRITE, words: [sector, 0, 0, 0] }, None, None);
        match answer {
            Ok(answer) if answer.message.label == corrode_abi::STATUS_OK => true,
            Ok(answer) => {
                println!("store      : the disk would not write sector {sector} (status {})", answer.message.label);
                false
            }
            Err(error) => {
                println!("store      : no answer writing sector {sector}: {error:?}");
                false
            }
        }
    })
}

/// Keep `theme` on the disk, if there is one.
pub fn save(theme: &Theme) {
    if !RUNNING.load(Ordering::Acquire) {
        return;
    }
    match call(store::SAVE, theme.to_words()) {
        Some(_) => println!("store      : kept the theme on the disk"),
        None => println!("store      : could not keep the theme on the disk"),
    }
}
