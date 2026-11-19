//! Where the desktop keeps its theme between boots: a virtio block device,
//! driven by a ring 3 task, `virtio-blk`, that holds its device, its queue
//! memory and an endpoint, and nothing else. See `corrode_abi::store`.
//!
//! No disk is the usual case, and fine: the theme is then whatever
//! /etc/desktop.conf says, every boot.

use core::sync::atomic::{AtomicBool, Ordering};

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
    let Some(queue) = memory::create(store::QUEUE_PAGES, false) else {
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
    crate::start_all(&[Some(driver)]);
    println!("store      : a disk at {}, driven in ring 3", found.address);
    RUNNING.store(true, Ordering::Release);
    true
}

fn call(label: u64, words: [u64; 4]) -> Option<[u64; 4]> {
    if !RUNNING.load(Ordering::Acquire) {
        return None;
    }
    let answer = ipc::call(STORE_ENDPOINT, ipc::Message { label, words }, None, None).ok()?;
    (answer.message.label == corrode_abi::STATUS_OK).then_some(answer.message.words)
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
