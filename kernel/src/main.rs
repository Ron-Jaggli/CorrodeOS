//! Corrode kernel entry point.
//!
//! Brings the machine up, runs the boot self-checks, starts the demo tasks,
//! and then becomes the painter: a loop that repaints whatever the running
//! animations made stale and sleeps when nothing is moving.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod cap;
mod device;
mod ipc;
mod irq;
mod limine;
mod mm;
mod modules;
mod pci;
mod serial;
mod smp;
mod surface;
mod sync;
mod syscall;
mod task;
mod time;

use core::sync::atomic::{AtomicU64, Ordering};

use arch::x86_64::{self, halt_forever, ioapic, lapic, pic, pit, vectors};
use corrode_render::{
    anim::{Animation, Ease, Fx, Spring, Velocity},
    blend::{self, Blend},
    scene::{Layer, LayerId, Painted, Scene},
    software::SoftwareRenderer,
    Color, PixelFormat, Rect, Renderer, SurfaceInfo,
};
use corrode_text::{Coverage, Style, Weight};

// Section boundaries emitted by kernel/linker.ld. Only their addresses are
// ever taken -- the values are meaningless.
#[allow(non_upper_case_globals)]
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

/// Scheduling tick rate. Both the LAPIC timer and the PIT run at this.
const TICK_HZ: u32 = 100;

const BACKGROUND: Color = Color::rgb(0x10, 0x12, 0x18);
const RUST: Color = Color::rgb(0xb7, 0x41, 0x0e);
const VEIL: Color = Color::rgb(0x9f, 0xc4, 0xe8);
const TITLE: Color = Color::rgb(0xf2, 0xee, 0xe8);
const CAPTION: Color = Color::rgb(0x8a, 0x90, 0x9c);

/// Entry point. Limine jumps here with paging on, in 64-bit long mode, on a
/// stack it provided. `kmain` never returns.
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    serial::init();

    println!();
    println!("Corrode v{}", env!("CARGO_PKG_VERSION"));
    println!("=================================");

    if let Some(info) = limine::BOOTLOADER_INFO.response.get() {
        println!("bootloader : {} {}", info.name(), info.version());
    } else {
        println!("bootloader : <no response>");
    }
    println!(
        "base rev 2 : {}",
        if limine::base_revision_supported() {
            "supported"
        } else {
            "NOT supported (running degraded)"
        }
    );

    // Descriptor tables before anything else: the IDT names a code selector
    // from our GDT, and until both are in place any fault is a silent reboot.
    x86_64::gdt::init();
    println!("gdt        : loaded (TSS + IST1 for #DF)");
    x86_64::idt::init();
    println!("idt        : 256 vectors installed");

    // Prove the trap path round-trips before trusting it with a real fault.
    x86_64::breakpoint();

    report_memory();
    init_frames();
    init_paging();
    init_heap();

    heap_test();

    // Interrupts last: everything they can reach has to exist first.
    //
    // The PIC still gets remapped even though we intend to mask it: a masked
    // 8259 can still emit a spurious IRQ7, and if that arrives on its power-on
    // vector it looks like a double fault.
    pic::init();
    println!("pit        : channel 0 at {} Hz", pit::init(TICK_HZ));

    if init_apic() {
        pic::disable();
    } else {
        println!("apic       : unavailable, falling back to the PIC");
        pic::unmask(pit::IRQ);
    }

    init_pci();

    // Still before interrupts: a handler inside the measurement window would
    // skew it, and nothing can run one yet.
    match time::init() {
        Some(clock) => println!(
            "clock      : tsc at {} MHz, {}",
            clock.hz / 1_000_000,
            if clock.invariant {
                "invariant"
            } else {
                "not advertised as invariant"
            }
        ),
        None => println!("clock      : no usable tsc, falling back to the {TICK_HZ} Hz tick"),
    }

    // SAFETY: the IDT has a handler for all 256 vectors, and every source we
    // have unmasked -- LAPIC timer, the PIT via one I/O APIC line, and the
    // spurious vector -- is dispatched explicitly.
    unsafe { x86_64::enable_interrupts() };

    timer_check();

    // Scheduler next: it adopts the current context as task 0, and from the
    // next timer tick onwards this function is running under preemption.
    task::init();

    // Other CPUs last of all, because an AP's first act is to become a task,
    // which needs the scheduler to exist. From here on "this CPU" is a real
    // question and the answer is `cpu::index()`.
    let (online, reported) = smp::start_aps();
    println!(
        "smp        : {} of {} cpu(s) online, {} Hz tick on each",
        online, reported, TICK_HZ
    );
    smp_check(online);

    // Seed the capability system, then check its invariants while execution is
    // still single-threaded and the output is deterministic.
    cap_bootstrap();
    cap_checks();
    pci_checks();
    anim_checks();
    blend_checks();
    text_checks();

    // Spawn, grant, then start. Masking interrupts would not do here: a task
    // created on this CPU can be picked up by another one, and disabling
    // interrupts binds only the core that does it. `start` is the ordering.
    let scribe = task::spawn("scribe", scribe_task);
    let mute = task::spawn("mute", mute_task);
    cap_grants(scribe, mute);
    start_all(&[scribe, mute]);

    let server = task::spawn("console-srv", console_server);
    let client = task::spawn("ipc-client", ipc_client);
    ipc_grants(server, client);
    start_all(&[server, client]);

    // Blocks until the server picks it up, which is the rendezvous working.
    cap_check(
        "bootstrap sent a Frame capability over IPC",
        ipc::send(
            BOOT_ENDPOINT,
            ipc::Message::from_text(LABEL_DONATE, "have a frame"),
            Some((8, cap::Rights::READ)),
        )
        .is_ok(),
    );

    // Ring 3. Spawn, grant, start, as above.
    #[cfg(not(feature = "user-test"))]
    {
        let user = task::spawn_user("user", user_program);
        user_grants(user);
        cap_check("spawned a ring 3 task", user.is_some());
        start_all(&[user]);
        serve_user();
        let service = call_user_service();
        check_address_spaces(user, service);
    }

    // Programs: ring 3 tasks that came from files rather than from the
    // kernel image. Then a device, driven by one.
    program_checks();
    #[cfg(not(feature = "user-test"))]
    device_checks();

    #[cfg(feature = "user-test")]
    user_fault_test();

    start_all(&[
        task::spawn("worker", worker_a),
        task::spawn("yielder", worker_b),
    ]);
    #[cfg(feature = "stack-test")]
    start_all(&[task::spawn("stack-bomb", stack_bomb)]);
    println!(
        "sched      : preemptive round-robin at {} Hz across {} cpu(s)",
        TICK_HZ,
        smp::online()
    );

    #[cfg(feature = "fault-test")]
    fault_test();

    #[cfg(feature = "wx-test")]
    wx_test();

    match init_renderer() {
        Some(mut renderer) => render_loop(&mut renderer),
        None => {
            println!("framebuffer: none -- serial only");
            println!();
            idle_loop()
        }
    }
}

/// Summarise the Limine memory map. Cheap, and it is the first thing you want
/// when the physical allocator starts misbehaving.
fn report_memory() {
    let Some(memmap) = limine::MEMMAP.response.get() else {
        println!("memory     : <no response>");
        return;
    };

    let mut usable = 0u64;
    let mut reclaimable = 0u64;
    for entry in memmap.entries() {
        match entry.kind {
            limine::MEMMAP_USABLE => usable += entry.length,
            limine::MEMMAP_BOOTLOADER_RECLAIMABLE => reclaimable += entry.length,
            _ => {}
        }
    }

    println!(
        "memory     : {} MiB usable, {} KiB bootloader-reclaimable, {} regions",
        usable / (1024 * 1024),
        reclaimable / 1024,
        memmap.entry_count
    );

    if let Some(hhdm) = limine::HHDM.response.get() {
        println!("hhdm       : {:#018x}", hhdm.offset);
    }
}

/// Build the physical frame allocator and check it round-trips.
fn init_frames() {
    let (Some(memmap), Some(hhdm)) = (limine::MEMMAP.response.get(), limine::HHDM.response.get())
    else {
        println!("frames     : unavailable (no memmap or hhdm)");
        return;
    };

    mm::frame::init(hhdm.offset, memmap.entries());

    let (free, total) = mm::frame::stats();
    println!(
        "frames     : {} free / {} total ({} MiB)",
        free,
        total,
        total as u64 * mm::frame::FRAME_SIZE / (1024 * 1024)
    );

    // Smoke test: take three frames, hand them back, confirm the count returns
    // to where it started. Cheap, and it catches a broken free list on the very
    // first boot rather than whenever paging starts failing.
    let before = free;
    let frames = [mm::frame::alloc(), mm::frame::alloc(), mm::frame::alloc()];
    match frames {
        [Some(a), Some(b), Some(c)] => {
            println!(
                "alloc      : {:#x} {:#x} {:#x}",
                a.addr(),
                b.addr(),
                c.addr()
            );
            for frame in [a, b, c] {
                // SAFETY: each frame came straight from `alloc` above, is freed
                // exactly once, and nothing ever referenced its contents.
                unsafe { mm::frame::free(frame) };
            }
            let (after, _) = mm::frame::stats();
            println!(
                "free list  : {} -> {} -> {} ({})",
                before,
                before - 3,
                after,
                if after == before {
                    "balanced"
                } else {
                    "LEAKED"
                }
            );
        }
        _ => println!("alloc      : FAILED -- allocator returned nothing"),
    }
}

/// Replace Limine's page tables with our own.
///
/// Ordering matters: this needs the frame allocator (tables come from it) and
/// must happen before interrupts are enabled, so nothing can preempt the CR3
/// switch.
fn init_paging() {
    let (Some(hhdm), Some(memmap), Some(kernel)) = (
        limine::HHDM.response.get(),
        limine::MEMMAP.response.get(),
        limine::KERNEL_ADDRESS.response.get(),
    ) else {
        println!("paging     : unavailable (missing limine response)");
        return;
    };

    // NX and WP must be on before we install entries that depend on them.
    // SAFETY: called once, before any page table entry using the NX bit exists.
    unsafe { x86_64::enable_memory_protections() };

    let Some(mut mapper) = mm::paging::Mapper::new(hhdm.offset) else {
        println!("paging     : out of frames building PML4");
        return;
    };

    // Direct-map everything physical, not just usable RAM: the framebuffer,
    // ACPI tables and Limine's own structures all live in non-usable regions we
    // still need to reach.
    let mut hhdm_pages = 0u64;
    let mut highest = 0u64;
    for entry in memmap.entries() {
        hhdm_pages += mapper.map_hhdm_region(entry.base, entry.length);
        highest = highest.max(entry.base + entry.length);
    }

    let virt_base = kernel.virtual_base;
    let phys_base = kernel.physical_base;

    let addr = |sym: &u8| sym as *const u8 as u64;
    // SAFETY: these are linker-emitted symbols; taking their addresses is the
    // only thing we do with them, and they are always in bounds of the image.
    let (text, rodata, data) = unsafe {
        (
            (addr(&__text_start), addr(&__text_end)),
            (addr(&__rodata_start), addr(&__rodata_end)),
            (addr(&__data_start), addr(&__data_end)),
        )
    };

    // W^X: executable and writable are never both set.
    let text_pages = mapper.map_kernel_range(text.0, text.1, virt_base, phys_base, 0);
    let rodata_pages = mapper.map_kernel_range(
        rodata.0,
        rodata.1,
        virt_base,
        phys_base,
        mm::paging::NO_EXECUTE,
    );
    let data_pages = mapper.map_kernel_range(
        data.0,
        data.1,
        virt_base,
        phys_base,
        mm::paging::WRITABLE | mm::paging::NO_EXECUTE,
    );

    println!(
        "paging     : pml4 {:#x}, hhdm {} x 2MiB ({} MiB mapped, top {:#x})",
        mapper.root(),
        hhdm_pages,
        hhdm_pages * 2,
        highest
    );
    println!(
        "sections   : text {} pages (RX), rodata {} (R+NX), data {} (RW+NX)",
        text_pages, rodata_pages, data_pages
    );

    // SAFETY: the new tables map the kernel image (so RIP stays valid), the
    // whole of physical memory through the HHDM (so the stack, the GDT/IDT and
    // the page tables themselves stay reachable), and the framebuffer.
    unsafe { mapper.activate() };

    let (free, total) = mm::frame::stats();
    println!(
        "paging     : CR3 loaded, {} frames spent ({} free / {})",
        total - free,
        free,
        total
    );

    // Keep the address space around: the heap maps through it next, and the
    // APIC will want MMIO mappings later.
    mm::paging::set_kernel_image_base(virt_base, phys_base);
    mm::paging::set_kernel_mapper(mapper);
}

/// Map the heap region and hand it to the allocator.
///
/// Runs after the CR3 switch, through the stored kernel mapper. Adding a
/// mapping for a previously-unmapped address needs no TLB flush -- there is no
/// stale entry to evict -- so doing this after activation is safe.
fn init_heap() {
    let pages = mm::paging::with_kernel_mapper(map_heap).unwrap_or(0);

    if pages == 0 {
        println!("heap       : no address space, heap unavailable");
        return;
    }
    if pages * 4096 < mm::heap::HEAP_SIZE {
        println!(
            "heap       : only {} of {} pages mapped",
            pages,
            mm::heap::HEAP_SIZE / 4096
        );
    }

    // SAFETY: `map_heap` just mapped exactly this region writable, nothing else
    // uses it, and this runs exactly once.
    unsafe { mm::heap::init(mm::heap::HEAP_START, pages * 4096) };

    let (free, total, _) = mm::heap::stats();
    println!(
        "heap       : {:#x} + {} KiB ({} KiB free)",
        mm::heap::HEAP_START,
        total / 1024,
        free / 1024
    );
}

/// Map the kernel heap, one 4 KiB frame at a time.
///
/// 4 KiB rather than 2 MiB because the frame allocator hands out single frames
/// with no contiguity guarantee, and a huge page needs 2 MiB of physically
/// contiguous memory.
fn map_heap(mapper: &mut mm::paging::Mapper) -> usize {
    let pages = mm::heap::HEAP_SIZE / 4096;
    for i in 0..pages {
        let Some(frame) = mm::frame::alloc() else {
            return i;
        };
        let va = mm::heap::HEAP_START as u64 + (i as u64) * 4096;
        if !mapper.map_4k(
            va,
            frame.addr(),
            mm::paging::WRITABLE | mm::paging::NO_EXECUTE | mm::paging::GLOBAL,
        ) {
            return i;
        }
    }
    pages
}

/// Exercise the heap and confirm it comes back clean.
///
/// The block count is the interesting number: one free block after everything
/// is dropped means coalescing actually merged the pieces back together. A
/// non-coalescing allocator would leave dozens and still report the right
/// number of free bytes.
fn heap_test() {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;

    let (before, _, blocks_before) = mm::heap::stats();

    let boxed = Box::new(0xc0ff_ee00_u64);
    let mut numbers: Vec<u64> = Vec::new();
    for i in 0..1024 {
        numbers.push(i * i);
    }
    let mut text = String::from("corrode");
    text.push_str(" heap");

    let (during, _, _) = mm::heap::stats();
    println!(
        "heap test  : box {:#x}, vec {} items (last {}), string {:?}",
        *boxed,
        numbers.len(),
        numbers[numbers.len() - 1],
        text.as_str()
    );

    drop(boxed);
    drop(numbers);
    drop(text);

    let (after, _, blocks_after) = mm::heap::stats();
    println!(
        "heap test  : {} -> {} -> {} bytes free ({})",
        before,
        during,
        after,
        if after == before {
            "balanced"
        } else {
            "LEAKED"
        }
    );
    println!(
        "coalescing : {} -> {} free blocks ({})",
        blocks_before,
        blocks_after,
        if blocks_after == 1 {
            "fully merged"
        } else {
            "FRAGMENTED"
        }
    );
}

/// Where the firmware's ACPI root pointer is, physically, and the direct map
/// offset to read it through.
fn rsdp() -> Option<(u64, u64)> {
    let (rsdp, hhdm) = (limine::RSDP.response.get()?, limine::HHDM.response.get()?);
    // Limine has handed this back as a physical address in some base revisions
    // and an HHDM pointer in others, so normalise rather than assume.
    let raw = rsdp.address;
    let physical = if raw >= hhdm.offset {
        raw - hhdm.offset
    } else {
        raw
    };
    Some((physical, hhdm.offset))
}

/// Bring up the APIC: parse the MADT, start the LAPIC timer, and route the PIT
/// through the I/O APIC.
///
/// Returns false if anything is missing, in which case the caller keeps the
/// PIC. Routing the PIT here as well as running the LAPIC timer is deliberate:
/// two independent counters advancing proves both the LAPIC *and* the I/O APIC
/// delivery paths, which a single timer would not.
fn init_apic() -> bool {
    let Some((rsdp_phys, hhdm)) = rsdp() else {
        println!("acpi       : no RSDP from the bootloader");
        return false;
    };

    // SAFETY: the bootloader guarantees this points at the firmware's RSDP, and
    // init_paging mapped every region the memory map reports, ACPI included.
    let Some(madt) = (unsafe { acpi::find_madt(rsdp_phys, hhdm) }) else {
        println!("acpi       : MADT not found or failed checksum");
        return false;
    };

    println!(
        "acpi       : MADT ok -- {} cpu(s), {} I/O APIC(s), {} override(s)",
        madt.cpu_count,
        madt.io_apics.len(),
        madt.overrides.len()
    );

    if !lapic::init(madt.lapic_address) {
        println!("lapic      : could not map registers");
        return false;
    }
    println!(
        "lapic      : id {} version {:#x} at {:#x}",
        lapic::id(),
        lapic::version(),
        madt.lapic_address
    );

    let rate = lapic::calibrate();
    if rate == 0 {
        println!("lapic      : calibration failed");
        return false;
    }
    let count = lapic::start_timer(vectors::LAPIC_TIMER, TICK_HZ);
    println!(
        "lapic      : timer {} Hz measured, {} Hz requested, count {}",
        rate, TICK_HZ, count
    );

    // Route the PIT. The MADT usually remaps ISA IRQ0 onto GSI 2; ignoring that
    // override is the classic reason a correctly-programmed timer never fires.
    let (gsi, active_low, level) = madt.gsi_for_irq(pit::IRQ);
    let Some(info) = madt.io_apic_for_gsi(gsi) else {
        println!("ioapic     : no controller owns GSI {}", gsi);
        return true;
    };
    let Some(io) = ioapic::IoApic::new(info.address, info.gsi_base) else {
        println!("ioapic     : could not map registers");
        return true;
    };

    // Firmware does not promise a quiet controller on handover.
    io.mask_all();
    println!(
        "ioapic     : id {} at {:#x}, {} entries from GSI {}",
        io.id(),
        info.address,
        io.entry_count(),
        io.gsi_base()
    );

    if io.route(gsi, vectors::IOAPIC_TIMER, lapic::id(), active_low, level) {
        println!(
            "ioapic     : IRQ{} -> GSI {} -> vector {:#x}{}",
            pit::IRQ,
            gsi,
            vectors::IOAPIC_TIMER,
            if gsi != pit::IRQ as u32 {
                " (via MADT override)"
            } else {
                ""
            }
        );
    } else {
        println!("ioapic     : GSI {} out of range", gsi);
    }

    if let Some((low, high)) = io.entry(gsi) {
        println!(
            "ioapic     : gsi{} entry lo {:#010x} hi {:#010x} (vec {:#x}, {}, {}, {})",
            gsi,
            low,
            high,
            low & 0xff,
            if low & (1 << 13) != 0 { "low" } else { "high" },
            if low & (1 << 15) != 0 {
                "level"
            } else {
                "edge"
            },
            if low & (1 << 16) != 0 {
                "MASKED"
            } else {
                "unmasked"
            },
        );
    }
    true
}

/// Walk the PCI bus and say what is on it.
///
/// Nothing here hands a device to anybody. The scan fills a table; authority
/// over an entry in it is a capability, minted later for whichever task is to
/// drive that device.
fn init_pci() {
    // SAFETY: as for the MADT -- the bootloader's RSDP, read through a direct
    // map that covers every region the memory map reports.
    let mcfg = rsdp().and_then(|(physical, hhdm)| unsafe { acpi::find_mcfg(physical, hhdm) });

    let Some(summary) = pci::init(mcfg) else {
        println!("pci        : no ECAM and no configuration ports -- no bus");
        return;
    };

    match summary.mechanism {
        pci::Mechanism::Ecam(ecam) => println!(
            "pci        : ECAM at {:#x} for buses {}..={} (ACPI MCFG), {} pages mapped",
            ecam.base,
            ecam.first_bus,
            ecam.last_bus,
            pci::ecam_pages()
        ),
        pci::Mechanism::Ports => {
            println!("pci        : no usable MCFG, configuration space through ports 0xcf8/0xcfc")
        }
    }

    for index in 0..summary.functions {
        let Some(device) = pci::device(index) else {
            continue;
        };
        print!(
            "pci        : {} {:04x}:{:04x} {}",
            device.address,
            device.header.vendor,
            device.header.device,
            device.header.class_name()
        );
        for (bar, decoded) in device.bars.iter().enumerate() {
            match *decoded {
                corrode_pci::Bar::Memory { address, size, .. } if size < 1024 => {
                    print!(", bar{bar} {size} bytes at {address:#x}")
                }
                corrode_pci::Bar::Memory { address, size, .. } => {
                    print!(", bar{bar} {} KiB at {address:#x}", size / 1024)
                }
                corrode_pci::Bar::Io { port, size } => {
                    print!(", bar{bar} {size} ports at {port:#x}")
                }
                corrode_pci::Bar::None => {}
            }
        }
        if let Some((_, size)) = device.rom {
            print!(", rom {} KiB", size / 1024);
        }
        if device.msi.is_some() {
            print!(", msi");
        }
        if let Some(msix) = device.msix {
            print!(", msi-x {}", msix.entries);
        }
        println!();
    }
    println!(
        "pci        : {} function(s) on {} bus(es)",
        summary.functions, summary.buses
    );

    check(
        "pci check",
        "the host bridge is at 00:00.0",
        pci::device(0).is_some_and(|device| {
            device.address == corrode_pci::Address::new(0, 0, 0, 0) && device.header.class == 0x06
        }),
    );
    // Two paths to the same registers that share no code below the decoder.
    // Skipped, not passed, on a machine with only one of them.
    if summary.cross_checked > 0 {
        check(
            "pci check",
            "ECAM and the port pair agree on every function",
            summary.agreed == summary.cross_checked && summary.agreed == summary.functions,
        );
    }
}

/// Bootstrap's capability to the first device on the bus, with every right.
const BOOT_PCI: cap::CapPtr = 16;
/// A read-only copy of it.
const BOOT_PCI_READ: cap::CapPtr = 17;
/// Bootstrap's capability to one BAR of some device.
const BOOT_BAR: cap::CapPtr = 18;

/// A device and its memory, as capabilities.
///
/// The scan is the kernel reading the bus for itself. These are the rules a
/// task is held to when it is handed one entry from that table.
fn pci_checks() {
    use cap::{CapError, ObjectKind, Rights};
    use corrode_pci::{ConfigSpace, BAR0, COMMAND, VENDOR_ID};

    let Some(first) = pci::device(0) else {
        return;
    };
    check(
        "pci check",
        "mint a PciDevice capability into slot 16",
        cap::mint(0, BOOT_PCI, cap::Object::PciDevice(0), Rights::ALL).is_ok()
            && cap::identify(BOOT_PCI) == Ok((ObjectKind::PciDevice, Rights::ALL)),
    );
    check(
        "pci check",
        "the capability reads the device the scan found",
        pci::config_read(BOOT_PCI, VENDOR_ID as u64, 2) == Ok(first.header.vendor as u32)
            && pci::config_read(BOOT_PCI, VENDOR_ID as u64, 4)
                == Ok((first.header.device as u32) << 16 | first.header.vendor as u32),
    );
    check(
        "pci check",
        "a Console capability is not a device",
        pci::config_read(0, 0, 4) == Err(CapError::WrongType),
    );
    check(
        "pci check",
        "reading past the end of config space is Invalid",
        pci::config_read(BOOT_PCI, 4096, 4) == Err(CapError::Invalid)
            && pci::config_read(BOOT_PCI, u64::MAX, 1) == Err(CapError::Invalid),
    );
    check(
        "pci check",
        "a misaligned or three-byte read is Invalid",
        pci::config_read(BOOT_PCI, 1, 2) == Err(CapError::Invalid)
            && pci::config_read(BOOT_PCI, 0, 3) == Err(CapError::Invalid),
    );

    let command = pci::config_read(BOOT_PCI, COMMAND as u64, 2);
    check(
        "pci check",
        "a read-only device capability cannot write",
        cap::copy(BOOT_PCI, BOOT_PCI_READ, Rights::READ) == Ok(Rights::READ)
            && command.is_ok_and(|value| {
                pci::config_write(BOOT_PCI_READ, COMMAND as u64, 2, value as u64)
                    == Err(CapError::InsufficientRights)
            }),
    );
    // Written with what it already holds: this is a check of the rule, not a
    // reason to reconfigure the host bridge.
    check(
        "pci check",
        "the command register is the holder's to write",
        command.is_ok_and(|value| {
            pci::config_write(BOOT_PCI, COMMAND as u64, 2, value as u64).is_ok()
                && pci::config_read(BOOT_PCI, COMMAND as u64, 2) == Ok(value)
        }),
    );
    let bar = pci::config_read(BOOT_PCI, BAR0 as u64, 4);
    check(
        "pci check",
        "a BAR is not, whatever rights are held",
        pci::config_write(BOOT_PCI, BAR0 as u64, 4, 0xffff_ffff) == Err(CapError::Invalid)
            && pci::config_read(BOOT_PCI, BAR0 as u64, 4) == bar,
    );

    // Memory: the first BAR of anything that has one.
    let Some(region) = pci::region(0) else {
        println!("pci        : no device has a memory BAR");
        return;
    };
    let Some(owner) = pci::device(region.device) else {
        return;
    };
    check(
        "pci check",
        "mint a DeviceMemory capability into slot 18",
        cap::mint(0, BOOT_BAR, cap::Object::DeviceMemory(0), Rights::ALL).is_ok()
            && cap::identify(BOOT_BAR) == Ok((ObjectKind::DeviceMemory, Rights::ALL)),
    );
    check(
        "pci check",
        "a BAR is a power of two in size, and aligned to it",
        region.len.is_power_of_two() && region.physical % region.len == 0,
    );
    // The register itself, read back now through the port pair if there is
    // one: sizing wrote all ones to it and should have left no trace.
    let low = owner.config().read32(BAR0 + 4 * region.bar as u16);
    check(
        "pci check",
        "sizing a BAR left it where the firmware put it",
        (low & !0xf) as u64 == region.physical & 0xffff_ffff,
    );
    check(
        "pci check",
        "device memory is not a device, nor the reverse",
        pci::config_read(BOOT_BAR, 0, 4) == Err(CapError::WrongType)
            && cap::device_memory_of(BOOT_PCI, Rights::READ).err() == Some(CapError::WrongType),
    );
}

/// Physical address of the frame minted into slot 8, so the checks can confirm
/// the capability reports the same one back.
static FRAME_CAP_ADDR: AtomicU64 = AtomicU64::new(0);

/// Bootstrap's endpoint capability: full rights, so it can hand out narrowed
/// send-only and receive-only copies.
const BOOT_ENDPOINT: cap::CapPtr = 9;

/// A second endpoint, served by the bootstrap task itself, for the ring 3 task.
/// Separate from the console server's so the two conversations cannot interleave
/// and make the boot log order-dependent.
const USER_ENDPOINT: cap::CapPtr = 10;
const USER_REPLY: cap::CapPtr = 11;

/// The user task's only capability: a send-only handle on [`USER_ENDPOINT`].
const USER_TASK_ENDPOINT: cap::CapPtr = 0;
/// A slot the user task holds nothing in, so it can demonstrate being denied.
const USER_TASK_EMPTY: cap::CapPtr = 5;

/// Labels the ring 3 task uses. It cannot print, so everything it wants
/// observed it has to say through a capability it was given.
const LABEL_USER_DENIED: u64 = 5;
const LABEL_USER_ECHO: u64 = 6;
/// Rights it got back from narrowing a capability in its own space.
const LABEL_USER_RIGHTS: u64 = 8;
/// Status of trying to pass on a capability it has no GRANT for.
const LABEL_USER_NOGRANT: u64 = 9;
/// A capability it is handing over.
const LABEL_USER_GIVE: u64 = 10;

/// Further slots in the user task's capability space.
const USER_TASK_FRAME: cap::CapPtr = 2;
const USER_TASK_COPY: cap::CapPtr = 3;

/// Where bootstrap puts a capability the ring 3 task sends it.
const USER_TRANSFER: cap::CapPtr = 13;

/// Messages the ring 3 task sends before it settles.
const USER_MESSAGES: usize = 6;

/// Eight bytes the user task asks the kernel to count, chosen so the expected
/// answer is simply its length.
const USER_PING: u64 = u64::from_le_bytes(*b"userping");
const USER_PING_LEN: u64 = 8;

/// A third endpoint, this one served *by* a ring 3 task and called by the
/// kernel. Bootstrap keeps the send half; the server gets receive-only.
const SERVICE_ENDPOINT: cap::CapPtr = 12;

/// Slots inside the ring 3 server's capability space.
const SERVICE_TASK_ENDPOINT: cap::CapPtr = 0;
const SERVICE_TASK_REPLY: cap::CapPtr = 1;

/// The service's protocol: add one to a number and hand it back.
///
/// Deliberately trivial. What is being demonstrated is not the arithmetic but
/// which side of the ring boundary it happened on.
const LABEL_INCREMENT: u64 = 7;
const SERVICE_INPUT: u64 = 41;

/// Slots inside the server's capability space.
const SERVER_ENDPOINT: cap::CapPtr = 0;
const SERVER_CONSOLE: cap::CapPtr = 1;
const SERVER_RECEIVE: cap::CapPtr = 2;
/// Where the kernel puts the one-shot capability to answer a caller.
const SERVER_REPLY: cap::CapPtr = 3;
/// A deliberate second reference to the same reply capability, kept so the
/// checks can prove that spending one copy kills every copy.
const SERVER_REPLY_COPY: cap::CapPtr = 4;

/// The client's only capability.
const CLIENT_ENDPOINT: cap::CapPtr = 0;

/// Messages the server expects before it exits.
const SERVER_MESSAGES: usize = 4;

/// The console server's protocol. A message's label is the operation; the
/// server dispatches on it rather than assuming every message means the same
/// thing.
const LABEL_PRINT: u64 = 1;
const LABEL_DONATE: u64 = 2;
/// Print, and answer with how many bytes were printed. The one request that
/// needs `call` rather than `send`, because it has a result.
const LABEL_COUNT: u64 = 3;
/// The server's answer to [`LABEL_COUNT`].
const LABEL_RESULT: u64 = 4;

/// Let a batch of freshly spawned tasks run, now that they have been granted
/// whatever they were going to be granted.
fn start_all(tasks: &[Option<usize>]) {
    for id in tasks.iter().flatten() {
        task::start(*id);
    }
}

/// Report one boot self-check.
fn check(subsystem: &str, name: &str, ok: bool) {
    println!(
        "{:<11}: {:<52}{}",
        subsystem,
        name,
        if ok { "PASS" } else { "FAIL" }
    );
}

/// Report one capability invariant.
fn cap_check(name: &str, ok: bool) {
    check("cap check", name, ok);
}

/// Exercise the animation formulas on the machine they will run on.
///
/// The render crate's own tests compare them against a numerical integration
/// of the spring equation, on the build host. These are the properties a
/// desktop relies on, checked here because this is where the arithmetic is
/// actually executed: integer-only, in ring 0, with no FPU state to lean on.
fn anim_checks() {
    let (from, to) = (Fx::from_int(0), Fx::from_int(1000));
    let bouncy = Animation::spring(0, from, to, Velocity::ZERO, Spring::BOUNCY);
    let snappy = Animation::spring(0, from, to, Velocity::ZERO, Spring::SNAPPY);

    check(
        "anim check",
        "a spring starts where it was released",
        bouncy.sample(0).value == from,
    );
    let end = bouncy.sample(bouncy.end_ns());
    check(
        "anim check",
        "a spring ends exactly on its target",
        end.done && end.value == to,
    );

    let peak = |animation: &Animation| {
        (0..400u64)
            .map(|step| animation.sample(step * 5_000_000).value)
            .max()
            .unwrap_or(from)
    };
    check(
        "anim check",
        "an underdamped spring overshoots",
        peak(&bouncy) > to,
    );
    check(
        "anim check",
        "a critically damped spring does not",
        peak(&snappy) <= to,
    );

    // The operation a desktop performs constantly: something already moving
    // is sent somewhere else. It must carry on from where it is, at the speed
    // it is going.
    const MID_FLIGHT: u64 = 120_000_000;
    let before = bouncy.sample(MID_FLIGHT);
    let turned = bouncy.retarget(MID_FLIGHT, Fx::from_int(-300), Spring::SNAPPY);
    let after = turned.sample(MID_FLIGHT);
    check(
        "anim check",
        "retargeting mid-flight keeps position and velocity",
        before.velocity.0 > 0
            && (before.value.0 - after.value.0).abs() <= 1
            && (before.velocity.0 - after.velocity.0).abs() <= 4,
    );

    // No state is advanced by sampling, so order cannot matter.
    let late = bouncy.sample(400_000_000);
    let early = bouncy.sample(100_000_000);
    check(
        "anim check",
        "sampling out of order gives the same answers",
        late != early && bouncy.sample(400_000_000) == late && bouncy.sample(100_000_000) == early,
    );

    println!(
        "anim       : over 1000 units, bouncy settles in {} ms and snappy in {} ms",
        bouncy.end_ns() / 1_000_000,
        snappy.end_ns() / 1_000_000
    );
}

/// Exercise alpha blending on the machine it will run on.
///
/// The host tests compare every source, destination and opacity against the
/// formula in floating point. These are the three properties a fade is judged
/// by on screen: it starts as nothing, ends as the colour itself, and leaves
/// no seam where it crosses something already that colour.
fn blend_checks() {
    let ends = (0..=255u8).all(|value| {
        blend::mix(value, !value, 255) == value && blend::mix(value, !value, 0) == !value
    });
    check(
        "blend check",
        "full and zero opacity are exact, not off by one",
        ends,
    );

    let seamless = (0..=255u8).all(|alpha| {
        [0u8, 1, 127, 254, 255]
            .iter()
            .all(|v| blend::mix(*v, *v, alpha) == *v)
    });
    check(
        "blend check",
        "a colour over itself is unchanged at any opacity",
        seamless,
    );

    // The path the rasteriser takes: all four bytes of a pixel in two
    // multiplies. 0x33 is exactly a fifth of the way from 0 to 0xff.
    let fifth = Blend::new(&PixelFormat::XRGB8888, Color::rgb(255, 100, 0), 51);
    check(
        "blend check",
        "a whole pixel blends the same as channel by channel",
        fifth.over(0x0000_64ff) == 0x0033_64cc,
    );
}

/// Exercise the text engine on the machine it will run on.
///
/// The host tests compare whole strings against the distance formula in
/// floating point. Here it is the two things that would go wrong first if the
/// integer version misbehaved in ring 0: ink landing outside the block that
/// was measured for it, and a stroke coming out the wrong thickness.
fn text_checks() {
    let rendered = |text: &str, style: Style| {
        let extent = corrode_text::measure(text, style);
        let mut coverage = alloc::vec![0u8; extent.area()];
        corrode_text::draw(
            text,
            style,
            &mut Coverage::new(&mut coverage, extent.width as usize),
        );
        (coverage, extent)
    };

    let (coverage, extent) = rendered("Corrode {jg}", Style::px(32).weight(Weight::Bold));
    let (width, height) = (extent.width as usize, extent.height as usize);
    let edge_clear = (0..width)
        .all(|x| coverage[x] == 0 && coverage[(height - 1) * width + x] == 0)
        && (0..height).all(|y| coverage[y * width] == 0 && coverage[y * width + width - 1] == 0);
    check(
        "text check",
        "letters are inked only inside what was measured",
        edge_clear && coverage.contains(&255),
    );

    // Coverage summed across a stem is its width in 255ths of a pixel. A
    // regular stroke is a twelfth of the em: 8 pixels at 96.
    let (coverage, extent) = rendered("I", Style::px(96));
    let row = (extent.baseline as usize - 20) * extent.width as usize;
    let stem: u32 = coverage[row..row + extent.width as usize]
        .iter()
        .map(|covered| *covered as u32)
        .sum();
    check(
        "text check",
        "a stem is as thick as its weight says, to 1/10 px",
        stem.abs_diff(8 * 255) <= 25,
    );
}

/// Create the system's first authority. The one call to `cap::mint`.
fn cap_bootstrap() {
    let result = cap::mint(0, 0, cap::Object::Console, cap::Rights::ALL);
    cap_check(
        "mint console capability into bootstrap slot 0",
        result.is_ok(),
    );

    cap_check(
        "mint an Endpoint capability into slot 9",
        cap::mint(
            0,
            BOOT_ENDPOINT,
            cap::Object::Endpoint(ipc::Endpoint::new()),
            cap::Rights::ALL,
        )
        .is_ok(),
    );

    cap_check(
        "mint the ring 3 task's Endpoint into slot 10",
        cap::mint(
            0,
            USER_ENDPOINT,
            cap::Object::Endpoint(ipc::Endpoint::new()),
            cap::Rights::ALL,
        )
        .is_ok(),
    );

    cap_check(
        "mint the ring 3 service's Endpoint into slot 12",
        cap::mint(
            0,
            SERVICE_ENDPOINT,
            cap::Object::Endpoint(ipc::Endpoint::new()),
            cap::Rights::ALL,
        )
        .is_ok(),
    );

    // Memory is an object like any other: a frame you hold, not a number you
    // happen to know.
    if let Some(frame) = mm::frame::alloc() {
        FRAME_CAP_ADDR.store(frame.addr(), Ordering::Relaxed);
        cap_check(
            "mint a Frame capability into slot 8",
            cap::mint(0, 8, cap::Object::Frame(frame.addr()), cap::Rights::ALL).is_ok(),
        );
    }
}

/// Exercise the rules the whole model rests on.
fn cap_checks() {
    use cap::{CapError, ObjectKind, Rights};

    cap_check(
        "slot 0 identifies as Console with rwg",
        cap::identify(0) == Ok((ObjectKind::Console, Rights::ALL)),
    );

    cap_check(
        "copy 0 -> 1 masked to READ yields r--",
        cap::copy(0, 1, Rights::READ) == Ok(Rights::READ),
    );

    // The invariant. Slot 1 holds r--; ask for rwg and you still get r--.
    cap_check(
        "copying r-- while requesting rwg still yields r--",
        cap::copy(1, 2, Rights::ALL) == Ok(Rights::READ),
    );

    cap_check(
        "write through an r-- capability is denied",
        cap::console_write(1, "this must never appear\n") == Err(CapError::InsufficientRights),
    );

    cap_check(
        "write through an rwg capability succeeds",
        cap::console_write(0, "").is_ok(),
    );

    cap_check(
        "empty slot reports Empty",
        cap::identify(7) == Err(CapError::Empty),
    );

    cap_check(
        "out-of-range slot reports OutOfRange",
        cap::identify(999) == Err(CapError::OutOfRange),
    );

    cap_check(
        "copy into an occupied slot reports Occupied",
        cap::copy(0, 1, Rights::ALL) == Err(CapError::Occupied),
    );

    cap_check(
        "delete frees the slot",
        cap::delete(1).is_ok() && cap::identify(1) == Err(CapError::Empty),
    );

    // Narrowing all the way to nothing is legal. The capability still names the
    // object; it just confers no authority over it.
    cap_check(
        "narrowing to NONE yields an empty rights set",
        cap::copy(0, 3, Rights::NONE).map(Rights::is_empty) == Ok(true),
    );
    cap_check(
        "a rights-less capability cannot write",
        cap::console_write(3, "this must never appear\n") == Err(CapError::InsufficientRights),
    );

    cap_check(
        "slot 8 identifies as Frame with rwg",
        cap::identify(8) == Ok((ObjectKind::Frame, Rights::ALL)),
    );
    cap_check(
        "Frame capability reports the address it was minted for",
        cap::frame_address(8) == Ok(FRAME_CAP_ADDR.load(Ordering::Relaxed)),
    );

    // Type confusion: holding a console capability does not let you use it as
    // memory, whatever rights it carries.
    cap_check(
        "using a Console capability as a Frame is WrongType",
        cap::frame_address(0) == Err(CapError::WrongType),
    );
}

/// Hand narrowed console capabilities to the two demo tasks.
fn cap_grants(scribe: Option<usize>, mute: Option<usize>) {
    use cap::Rights;

    if let Some(id) = scribe {
        cap_check(
            "grant scribe a rw- console capability",
            cap::grant(id, 0, 0, Rights::READ_WRITE) == Ok(Rights::READ_WRITE),
        );
    }
    if let Some(id) = mute {
        cap_check(
            "grant mute an r-- console capability",
            cap::grant(id, 0, 0, Rights::READ) == Ok(Rights::READ),
        );
    }

    // A task is an object too, so "may inspect that task" is a capability
    // rather than something every caller implicitly has.
    if let Some(id) = scribe {
        cap_check(
            "mint a Task capability naming scribe",
            cap::mint(0, 6, cap::Object::Task(id), Rights::READ).is_ok(),
        );
        cap_check(
            "slot 6 identifies as Task with r--",
            cap::identify(6) == Ok((cap::ObjectKind::Task, Rights::READ)),
        );
        cap_check("Task capability names scribe", cap::task_of(6) == Ok(id));
    }

    println!(
        "cap        : slots held -- bootstrap {}, scribe {}, mute {}",
        cap::slots_used(0).unwrap_or(0),
        scribe.and_then(cap::slots_used).unwrap_or(0),
        mute.and_then(cap::slots_used).unwrap_or(0),
    );
}

/// Split the endpoint into a receive-only half for the server and a send-only
/// half for the client. Same object, different authority.
fn ipc_grants(server: Option<usize>, client: Option<usize>) {
    use cap::Rights;

    if let Some(id) = server {
        cap_check(
            "grant server a receive-only endpoint capability",
            cap::grant(id, BOOT_ENDPOINT, SERVER_ENDPOINT, Rights::READ) == Ok(Rights::READ),
        );
        cap_check(
            "grant server a rw- console capability",
            cap::grant(id, 0, SERVER_CONSOLE, Rights::READ_WRITE) == Ok(Rights::READ_WRITE),
        );
    }
    if let Some(id) = client {
        cap_check(
            "grant client a send-only endpoint capability",
            cap::grant(id, BOOT_ENDPOINT, CLIENT_ENDPOINT, Rights::WRITE) == Ok(Rights::WRITE),
        );
    }
}

/// Holds the only writable console capability, plus a receive-only endpoint.
///
/// This is the microkernel argument in miniature: the console is a service, and
/// a task that wants output asks for it rather than having it.
extern "C" fn console_server() {
    // The endpoint capability is receive-only, so this task cannot send on the
    // very endpoint it serves -- it cannot impersonate a client.
    cap_check(
        "server cannot send on a receive-only endpoint",
        ipc::send(SERVER_ENDPOINT, ipc::Message::EMPTY, None)
            == Err(cap::CapError::InsufficientRights),
    );

    for _ in 0..SERVER_MESSAGES {
        let received = match ipc::recv(SERVER_ENDPOINT, Some(SERVER_RECEIVE), Some(SERVER_REPLY)) {
            Ok(received) => received,
            Err(error) => {
                println!("ipc        : server recv failed: {:?}", error);
                return;
            }
        };

        let mut buffer = [0u8; ipc::TEXT_CAPACITY];
        let length = received.message.text_bytes(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap_or("<non-utf8>");

        match received.message.label {
            LABEL_PRINT => {
                // Every byte of this goes out through the capability, not
                // through println!. A server without a console capability would
                // print nothing at all.
                let line = alloc::format!(
                    "ipc        : print from task {}: {:?}\n",
                    received.from,
                    text
                );
                let _ = cap::console_write(SERVER_CONSOLE, &line);

                // Nobody is waiting on a one-way send, so there is nothing to
                // answer and the kernel issues no authority to answer it.
                cap_check(
                    "a one-way send carries no reply capability",
                    received.reply.is_none(),
                );
            }

            LABEL_DONATE => {
                let line = alloc::format!(
                    "ipc        : donation from task {}: {:?}\n",
                    received.from,
                    text
                );
                let _ = cap::console_write(SERVER_CONSOLE, &line);

                match received.capability {
                    Some((kind, rights)) => {
                        let line = alloc::format!(
                            "ipc        : received a {kind:?} capability with {rights:?}\n"
                        );
                        let _ = cap::console_write(SERVER_CONSOLE, &line);

                        cap_check(
                            "transferred Frame capability is usable by the server",
                            cap::frame_address(SERVER_RECEIVE).is_ok(),
                        );
                        cap_check(
                            "transferred capability arrived narrowed to r--",
                            rights == cap::Rights::READ,
                        );
                    }
                    None => cap_check("donation carried a capability", false),
                }
            }

            LABEL_COUNT => {
                let line = alloc::format!(
                    "ipc        : call from task {}: {:?}\n",
                    received.from,
                    text
                );
                let _ = cap::console_write(SERVER_CONSOLE, &line);

                let Some(slot) = received.reply else {
                    cap_check("call arrived with a reply capability", false);
                    continue;
                };
                cap_check(
                    "reply capability arrived as Reply with -w-",
                    cap::identify(slot) == Ok((cap::ObjectKind::Reply, cap::Rights::WRITE)),
                );
                cap_check(
                    "a reply capability cannot be handed on without GRANT",
                    cap::grant(received.from, slot, 9, cap::Rights::WRITE)
                        == Err(cap::CapError::InsufficientRights),
                );

                // Keep a second reference before spending the first. Copying
                // inside our own space needs no GRANT -- and buys nothing,
                // which is exactly what the check below demonstrates.
                let copied = cap::copy(slot, SERVER_REPLY_COPY, cap::Rights::WRITE).is_ok();

                cap_check(
                    "server answered the call",
                    ipc::reply(
                        slot,
                        ipc::Message::from_word(LABEL_RESULT, length as u64),
                        None,
                    )
                    .is_ok(),
                );
                cap_check(
                    "replying consumed the capability's slot",
                    cap::identify(slot) == Err(cap::CapError::Empty),
                );
                cap_check(
                    "a kept copy of a spent reply capability is stale",
                    copied
                        && ipc::reply(SERVER_REPLY_COPY, ipc::Message::EMPTY, None)
                            == Err(cap::CapError::Stale),
                );
                let _ = cap::delete(SERVER_REPLY_COPY);
            }

            other => {
                let line = alloc::format!("ipc        : unknown label {other}\n");
                let _ = cap::console_write(SERVER_CONSOLE, &line);
            }
        }
    }

    // One last receive, declared one-way only by passing no reply slot. A call
    // arriving here is declined and its caller told; this server is not woken
    // for it, and the plain send queued behind it is served as normal.
    match ipc::recv(SERVER_ENDPOINT, None, None) {
        Ok(received) => {
            let mut buffer = [0u8; ipc::TEXT_CAPACITY];
            let length = received.message.text_bytes(&mut buffer);
            let text = core::str::from_utf8(&buffer[..length]).unwrap_or("<non-utf8>");
            let line = alloc::format!("ipc        : one-way server took {text:?}\n");
            let _ = cap::console_write(SERVER_CONSOLE, &line);
            cap_check(
                "a one-way-only receive still serves plain sends",
                received.reply.is_none() && received.message.label == LABEL_PRINT,
            );
        }
        Err(error) => {
            println!("ipc        : one-way recv failed: {:?}", error);
            cap_check("a one-way-only receive still serves plain sends", false);
        }
    }
}

/// Holds one send-only endpoint capability and nothing else.
extern "C" fn ipc_client() {
    cap_check(
        "client holds no console capability at all",
        cap::console_write(1, "this must never appear\n") == Err(cap::CapError::Empty),
    );
    cap_check(
        "client cannot receive on a send-only endpoint",
        ipc::recv(CLIENT_ENDPOINT, None, None).is_err(),
    );
    cap_check(
        "client cannot reply without a reply capability",
        ipc::reply(1, ipc::Message::EMPTY, None) == Err(cap::CapError::Empty),
    );

    for text in ["hello from the client", "and a second message"] {
        let result = ipc::send(
            CLIENT_ENDPOINT,
            ipc::Message::from_text(LABEL_PRINT, text),
            None,
        );
        cap_check("client send rendezvoused with the server", result.is_ok());
    }

    // The same send-only capability, used to ask a question rather than make a
    // statement. Being answered needs no extra authority: the client is already
    // blocked, and the reply is the other half of a send it could already make.
    const QUESTION: &str = "how long is this?";
    match ipc::call(
        CLIENT_ENDPOINT,
        ipc::Message::from_text(LABEL_COUNT, QUESTION),
        None,
        None,
    ) {
        Ok(answer) => {
            cap_check(
                "client call returned the server's answer",
                answer.message.label == LABEL_RESULT
                    && answer.message.words[0] == QUESTION.len() as u64,
            );
        }
        Err(error) => {
            println!("ipc        : client call failed: {:?}", error);
            cap_check("client call returned the server's answer", false);
        }
    }

    // The server's next receive takes no calls. Whichever of us reaches the
    // endpoint first, the answer is the same: the call is refused rather than
    // parked forever, and the send behind it still gets through.
    cap_check(
        "calling a one-way-only server is refused, not hung",
        ipc::call(
            CLIENT_ENDPOINT,
            ipc::Message::from_text(LABEL_COUNT, "nobody can answer this"),
            None,
            None,
        )
        .err()
            == Some(cap::CapError::Refused),
    );
    cap_check(
        "a send behind a refused call is still delivered",
        ipc::send(
            CLIENT_ENDPOINT,
            ipc::Message::from_text(LABEL_PRINT, "one-way still works"),
            None,
        )
        .is_ok(),
    );
}

/// A program that runs in **ring 3**.
///
/// Naked assembly, and it has to be: this function's frames get mapped a second
/// time at a user address, and nothing else in the kernel does. A call to
/// another function or a `&str` in `.rodata` would be a reference to an address
/// ring 3 cannot reach, and the compiler has no way to be told that. Hand-written
/// assembly is the only way to be sure the code touches nothing but its own page
/// and its own stack.
///
/// It holds exactly one capability -- a send-only endpoint -- and so the only
/// way it can report anything at all is to say it through that. Which is the
/// whole argument: a task with an empty capability space is not restricted, it
/// is inert.
#[unsafe(naked)]
extern "C" fn user_program() {
    core::arch::naked_asm!(
        // Try to call through a slot holding nothing. Expect a refusal, and
        // keep it: this is the only evidence we can produce that being denied
        // is what actually happened.
        "mov rax, {call}",
        "mov rdi, {empty_slot}",
        "mov rsi, {label_count}",
        "xor rdx, rdx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {no_slot}",
        "int {gate}",
        "mov rbx, rax",

        // Report that refusal through the capability we do hold.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_denied}",
        "mov rdx, rbx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        // No capability rides along with this one.
        "mov r12, {no_slot}",
        "int {gate}",

        // A real request, and one with an answer: how long is this word?
        "mov rax, {call}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_count}",
        "movabs rdx, {ping}",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {no_slot}",
        "int {gate}",
        "mov rbx, rdx",

        // Echo the answer back, so the kernel can see we received it.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_echo}",
        "mov rdx, rbx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {no_slot}",
        "int {gate}",

        // Narrow our own endpoint capability into a spare slot, asking for
        // every right. We hold -w-, so that is what we must get back.
        "mov rax, {copy}",
        "mov rdi, {endpoint}",
        "mov rsi, {copy_slot}",
        "mov rdx, {rights_all}",
        "int {gate}",
        "mov rbx, rdx",

        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_rights}",
        "mov rdx, rbx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {no_slot}",
        "int {gate}",

        // Try to pass that copy on. It carries no GRANT, so this must fail --
        // and failing means it sends nothing, so the result has to be reported
        // by a second message that transfers nothing.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_give}",
        "xor rdx, rdx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {copy_slot}",
        "mov r13, {rights_all}",
        "int {gate}",
        "mov rbx, rax",

        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_nogrant}",
        "mov rdx, rbx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {no_slot}",
        "int {gate}",

        // Now hand over something we *were* given with GRANT, narrowed on the
        // way out to read-only.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_give}",
        "xor rdx, rdx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "mov r12, {frame_slot}",
        "mov r13, {rights_read}",
        "int {gate}",

        // Done talking. Stay a live ring 3 task in the rotation rather than
        // returning -- there is no ring 3 runtime to return to.
        "2:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 2b",

        call = const syscall::SYS_CALL,
        send = const syscall::SYS_SEND,
        copy = const syscall::SYS_COPY,
        yield_op = const syscall::SYS_YIELD,
        no_slot = const syscall::NO_SLOT,
        gate = const vectors::SYSCALL,
        endpoint = const USER_TASK_ENDPOINT,
        empty_slot = const USER_TASK_EMPTY,
        frame_slot = const USER_TASK_FRAME,
        copy_slot = const USER_TASK_COPY,
        rights_all = const cap::Rights::ALL.bits(),
        rights_read = const cap::Rights::READ.bits(),
        label_count = const LABEL_COUNT,
        label_denied = const LABEL_USER_DENIED,
        label_echo = const LABEL_USER_ECHO,
        label_rights = const LABEL_USER_RIGHTS,
        label_nogrant = const LABEL_USER_NOGRANT,
        label_give = const LABEL_USER_GIVE,
        ping = const USER_PING,
    )
}

/// A **service** that runs in ring 3.
///
/// This is the shape every userspace driver has to take: receive, act, answer,
/// repeat. It holds a receive-only endpoint capability, so it cannot send on
/// the endpoint it serves and therefore cannot impersonate one of its own
/// clients -- and it still answers them, because a reply capability is separate
/// authority that the kernel hands over per call.
#[unsafe(naked)]
extern "C" fn user_service() {
    core::arch::naked_asm!(
        // No capability rides along with anything this task sends or answers.
        "mov r12, {no_slot}",
        "2:",
        // Receive. No slot for a transferred capability, but one for the
        // capability that lets us answer.
        "mov rax, {recv}",
        "mov rdi, {endpoint}",
        "mov rsi, {no_slot}",
        "mov rdx, {reply_slot}",
        "int {gate}",
        // Anything other than success means there is nobody to answer, so
        // going round again would spin. Park instead.
        "test rax, rax",
        "jnz 3f",

        // The work: add one. rdx holds word 0 of what arrived.
        "mov rbx, rdx",

        // Before answering, try to *send* on the endpoint we serve. We hold it
        // read-only, so this must be refused -- a server that could send on its
        // own endpoint could answer its clients' requests by impersonating one
        // of them. r14 rather than rcx or r11 to hold the result: those two are
        // the registers fast syscalls clobber, and r12 names a capability to
        // send.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "xor rsi, rsi",
        "xor rdx, rdx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "int {gate}",
        "mov r14, rax",

        // Answer, carrying both the result and the status of that attempt --
        // the only way a task with no console can report on itself.
        "mov rax, {reply}",
        "mov rdi, {reply_slot}",
        "mov rsi, {label_result}",
        "lea rdx, [rbx + 1]",
        "mov r10, r14",
        "xor r8, r8",
        "xor r9, r9",
        "int {gate}",
        "jmp 2b",

        "3:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 3b",

        recv = const syscall::SYS_RECV,
        reply = const syscall::SYS_REPLY,
        send = const syscall::SYS_SEND,
        yield_op = const syscall::SYS_YIELD,
        no_slot = const syscall::NO_SLOT,
        gate = const vectors::SYSCALL,
        endpoint = const SERVICE_TASK_ENDPOINT,
        reply_slot = const SERVICE_TASK_REPLY,
        label_result = const LABEL_RESULT,
    )
}

/// Hand the ring 3 service the receive half of its endpoint, and call it.
///
/// The kernel is the client here, which is the microkernel argument at its
/// sharpest: the privileged side asking an unprivileged task to do work, and
/// having no way to do it except by holding a capability to that task's
/// endpoint.
fn call_user_service() -> Option<usize> {
    let service = task::spawn_user("user-srv", user_service);
    if let Some(id) = service {
        cap_check(
            "grant the ring 3 service a receive-only endpoint capability",
            cap::grant(
                id,
                SERVICE_ENDPOINT,
                SERVICE_TASK_ENDPOINT,
                cap::Rights::READ,
            ) == Ok(cap::Rights::READ),
        );
    }
    start_all(&[service]);

    if service.is_none() {
        cap_check("spawned a ring 3 service", false);
        return None;
    }

    match ipc::call(
        SERVICE_ENDPOINT,
        ipc::Message::from_word(LABEL_INCREMENT, SERVICE_INPUT),
        None,
        None,
    ) {
        Ok(answer) => {
            println!(
                "service    : kernel asked ring 3 for {} + 1, got {}",
                SERVICE_INPUT, answer.message.words[0]
            );
            cap_check(
                "a ring 3 server answered a call from the kernel",
                answer.message.label == LABEL_RESULT
                    && answer.message.words[0] == SERVICE_INPUT + 1,
            );
            cap_check(
                "the ring 3 server cannot send on the endpoint it serves",
                answer.message.words[1] == cap::CapError::InsufficientRights.code(),
            );
        }
        Err(error) => {
            println!("service    : call into ring 3 failed: {:?}", error);
            cap_check("a ring 3 server answered a call from the kernel", false);
        }
    }

    service
}

/// Bootstrap's end of the driver's endpoint, and its capabilities to the test
/// device, one BAR of it, and its interrupt.
const PROBE_ENDPOINT: cap::CapPtr = 19;
const BOOT_EDU: cap::CapPtr = 20;
const BOOT_EDU_BAR: cap::CapPtr = 21;
const BOOT_EDU_IRQ: cap::CapPtr = 22;
/// A copy of the interrupt capability with nothing on it.
const BOOT_EDU_IRQ_NONE: cap::CapPtr = 23;
/// Bootstrap's end of the log: every right, to hand out send-only copies.
const LOG_ENDPOINT: cap::CapPtr = 24;

/// Reports the `edu` driver makes before it settles.
const PROBE_MESSAGES: usize = 4;

/// The log server's two capabilities.
const LOG_TASK_ENDPOINT: cap::CapPtr = 0;
const LOG_TASK_CONSOLE: cap::CapPtr = 1;

/// Most bytes of one line the log will hold for a program before printing
/// what it has. A program cannot make the kernel remember more than this.
const LOG_LINE_MAX: usize = 240;

/// Where ring 3 programs print.
///
/// A kernel task holding the receive half of one endpoint and a console
/// capability. A program that was handed the send half can put a line on the
/// console; a program that was not cannot, and nothing in the runtime changes
/// that. Lines arrive thirty-two bytes at a time, because that is what fits
/// in a message, and are printed whole under the name of the task that sent
/// them -- which the kernel knows, and the sender cannot fake.
extern "C" fn log_server() {
    use alloc::string::String;
    use alloc::vec::Vec;
    use corrode_abi::log;

    // One unfinished line per sender.
    let mut partial: Vec<(usize, String)> = Vec::new();

    loop {
        let Ok(received) = ipc::recv(LOG_TASK_ENDPOINT, None, None) else {
            return;
        };
        let mut buffer = [0u8; ipc::TEXT_CAPACITY];
        let length = received.message.text_bytes(&mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap_or("?");

        let index = match partial.iter().position(|(from, _)| *from == received.from) {
            Some(index) => index,
            None => {
                partial.push((received.from, String::new()));
                partial.len() - 1
            }
        };
        let line = &mut partial[index].1;
        if line.len() + text.len() <= LOG_LINE_MAX {
            line.push_str(text);
        }

        if received.message.label == log::LINE {
            let name = task::name_of(received.from).unwrap_or("?");
            let out = alloc::format!("{:<11}: {}\n", name, line);
            let _ = cap::console_write(LOG_TASK_CONSOLE, &out);
            partial.swap_remove(index);
        }
    }
}

/// Start the log server, and keep the endpoint to hand out.
fn start_log() {
    use cap::Rights;

    let minted = cap::mint(
        0,
        LOG_ENDPOINT,
        cap::Object::Endpoint(ipc::Endpoint::new()),
        Rights::ALL,
    )
    .is_ok();
    let server = task::spawn("log", log_server);
    let granted = server.is_some_and(|id| {
        cap::grant(id, LOG_ENDPOINT, LOG_TASK_ENDPOINT, Rights::READ).is_ok()
            && cap::grant(id, 0, LOG_TASK_CONSOLE, Rights::READ_WRITE).is_ok()
    });
    check(
        "program",
        "start a log server holding the console",
        minted && granted,
    );
    start_all(&[server]);
}

fn program_check(name: &str, ok: bool) {
    check("program", name, ok);
}

/// Load a program from the bootloader's modules and make a task of it.
///
/// Says why not, if not: a missing module is a boot medium built without
/// it, and a refused one is a file the loader will not run.
fn spawn_program(name: &'static str) -> Option<usize> {
    let Some(module) = modules::find(name) else {
        println!("program    : no module called {name:?} was loaded");
        return None;
    };
    match task::spawn_program(name, module.bytes) {
        Ok(id) => Some(id),
        Err(error) => {
            println!("program    : {name:?} was not loaded: {error:?}");
            None
        }
    }
}

/// What the bootloader left in RAM, and the loader's rules on the target.
///
/// The host tests feed the ELF reader files that lie. These are the things
/// only a boot can show: that a real program built by the real toolchain
/// loads, and that what ends up in its page tables is what its file asked
/// for and no more.
fn program_checks() {
    use mm::paging::{NO_EXECUTE, USER, WRITABLE};

    let mut count = 0;
    for module in modules::all() {
        println!(
            "module     : {} -- {} KiB at {:#x}",
            module.path,
            module.bytes.len().div_ceil(1024),
            module.physical
        );
        count += 1;
    }
    if count == 0 {
        println!("module     : none -- the boot medium carries no programs");
        return;
    }

    start_log();

    // A file that is not a program, straight to the loader.
    program_check(
        "a file that is not a program is refused",
        matches!(
            task::user::load(b"not an ELF file, whatever its name says"),
            Err(task::user::LoadError::Format(_))
        ),
    );

    // A program that reads kernel memory. It gets nothing at all: no
    // capability, so no way to say anything, and it does not need one.
    let Some(module) = modules::find("fault") else {
        return;
    };
    let Some(bomb) = spawn_program("fault") else {
        program_check("load a program from a module", false);
        return;
    };
    program_check(
        "load a program from a module",
        cap::slots_used(bomb) == Some(0),
    );

    if let (Some(space), Ok(elf)) = (
        task::space_of(bomb),
        corrode_elf::Elf::parse(
            module.bytes,
            corrode_abi::layout::IMAGE_BASE..corrode_abi::layout::IMAGE_END,
        ),
    ) {
        // Every page of every segment, against the flags its file asked for.
        let honoured = elf.segments().all(|segment| {
            segment.pages().step_by(4096).all(|page| {
                space.leaf_flags(page).is_some_and(|flags| {
                    flags & USER != 0
                        && (flags & WRITABLE != 0) == segment.writable
                        && (flags & NO_EXECUTE == 0) == segment.executable
                })
            })
        });
        program_check(
            "its pages carry the permissions its file asked for",
            honoured,
        );
        let exclusive = elf.segments().all(|segment| {
            segment.pages().step_by(4096).all(|page| {
                space
                    .leaf_flags(page)
                    .is_some_and(|flags| flags & WRITABLE == 0 || flags & NO_EXECUTE != 0)
            })
        });
        program_check("no page of it is writable and executable", exclusive);

        let entry = space.translate(elf.entry());
        let file = (module.physical..module.physical + module.bytes.len() as u64)
            .contains(&entry.unwrap_or(0));
        program_check(
            "it runs from its own frames, not the file's",
            entry.is_some() && !file,
        );
    }

    #[cfg(not(feature = "user-test"))]
    {
        println!("program    : starting one that reads kernel memory (expect a task fault)");
        start_all(&[Some(bomb)]);
        // Wait for it to be stopped. Bounded: if the fault were somehow
        // survivable this would otherwise sit here for good.
        let deadline = time::now_ns() + 2_000_000_000;
        while task::state_of(bomb) != Some(task::State::Finished) && time::now_ns() < deadline {
            task::yield_now();
        }
        program_check(
            "a program that faults is stopped",
            task::state_of(bomb) == Some(task::State::Finished),
        );
        // This line being printed at all is the other half.
        program_check("and the machine is not", true);
    }
}

fn device_check(name: &str, ok: bool) {
    check("device", name, ok);
}

/// Hand one device to one ring 3 task and watch it drive it.
///
/// The device is QEMU's `edu`, which exists to be poked: a register that
/// answers with what was written inverted, one that raises an interrupt when
/// told to, and one that works out a factorial and interrupts when it has.
/// On a machine without it there is nothing to check and this says so.
fn device_checks() {
    use cap::{CapError, Rights};

    use corrode_abi::edu;

    let Some((index, found)) = pci::find(|device| {
        device.header.vendor == edu::VENDOR && device.header.device == edu::DEVICE_ID
    }) else {
        println!("device     : no edu test device on the bus -- nothing to drive");
        return;
    };
    let Some(region) = pci::region_of(index, 0) else {
        println!("device     : the edu device has no memory BAR");
        return;
    };
    let Some(bar) = pci::region(region) else {
        return;
    };

    // The task first: its interrupt is aimed at the CPU it is pinned to.
    let Some(driver) = spawn_program("edu") else {
        device_check("load the driver from a module", false);
        return;
    };
    let line = task::cpu_of(driver).and_then(|cpu| irq::create(index, 0, cpu));

    let minted = cap::mint(
        0,
        PROBE_ENDPOINT,
        cap::Object::Endpoint(ipc::Endpoint::new()),
        Rights::ALL,
    )
    .is_ok()
        && cap::mint(0, BOOT_EDU, cap::Object::PciDevice(index), Rights::ALL).is_ok()
        && cap::mint(
            0,
            BOOT_EDU_BAR,
            cap::Object::DeviceMemory(region),
            Rights::ALL,
        )
        .is_ok()
        && line.is_some_and(|line| {
            cap::mint(0, BOOT_EDU_IRQ, cap::Object::Interrupt(line), Rights::ALL).is_ok()
        });
    device_check("mint a device, its memory and its interrupt", minted);
    let Some(line) = line else {
        println!(
            "device     : no interrupt line for {} -- it has no MSI",
            found.address
        );
        return;
    };

    // The rules, tried from this side before anybody else holds anything.
    device_check(
        "a kernel task has nowhere to map device memory",
        device::map(BOOT_EDU_BAR).err() == Some(CapError::NoSpace),
    );
    device_check(
        "device memory is not an interrupt",
        irq::wait(BOOT_EDU_BAR) == Err(CapError::WrongType),
    );
    device_check(
        "an interrupt capability without READ cannot wait",
        cap::copy(BOOT_EDU_IRQ, BOOT_EDU_IRQ_NONE, Rights::WRITE) == Ok(Rights::WRITE)
            && irq::wait(BOOT_EDU_IRQ_NONE) == Err(CapError::InsufficientRights),
    );

    let granted = cap::grant(driver, PROBE_ENDPOINT, edu::REPORT, Rights::WRITE).is_ok()
        && cap::grant(driver, BOOT_EDU, edu::DEVICE, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, BOOT_EDU_BAR, edu::BAR, Rights::READ_WRITE).is_ok()
        && cap::grant(driver, BOOT_EDU_IRQ, edu::IRQ, Rights::READ).is_ok()
        && cap::grant(driver, LOG_ENDPOINT, edu::LOG, Rights::WRITE).is_ok();
    device_check(
        "grant the driver its device and somewhere to report",
        granted && cap::slots_used(driver) == Some(5),
    );
    start_all(&[Some(driver)]);

    for _ in 0..PROBE_MESSAGES {
        let received = match ipc::recv(PROBE_ENDPOINT, None, None) {
            Ok(received) => received,
            Err(error) => {
                println!("device     : recv failed: {:?}", error);
                return;
            }
        };
        let words = received.message.words;

        match received.message.label {
            edu::LABEL_CONFIG => {
                println!(
                    "device     : ring 3 read {:04x}:{:04x} from its configuration space",
                    words[0] & 0xffff,
                    words[0] >> 16
                );
                device_check(
                    "ring 3 read its device's identity",
                    words[0] == (edu::DEVICE_ID as u64) << 16 | edu::VENDOR as u64,
                );
                device_check(
                    "ring 3 cannot move its device's BAR",
                    words[1] == CapError::Invalid.code(),
                );
                device_check(
                    "ring 3 turned on decoding and bus mastering",
                    words[2] == syscall::STATUS_OK
                        && pci::config_read(BOOT_EDU, corrode_pci::COMMAND as u64, 2)
                            .is_ok_and(|command| command & 0x6 == 0x6),
                );
            }

            edu::LABEL_MEMORY => {
                println!(
                    "device     : {} KiB of registers at {:#x} in ring 3, identifying as {:#010x}",
                    words[2] / 1024,
                    words[3],
                    words[0]
                );
                device_check(
                    "the kernel chose where the registers went",
                    words[3] == device::user_address(region) && words[2] == bar.len,
                );
                device_check(
                    "ring 3 read a device register",
                    words[0] == edu::IDENT as u64,
                );
                device_check(
                    "ring 3 wrote one, and the device answered",
                    words[1] == !edu::PROBE as u64,
                );
                let theirs = task::space_of(driver)
                    .and_then(|space| space.translate(device::user_address(region)));
                device_check(
                    "that address is the BAR, by its page tables",
                    theirs == Some(bar.physical),
                );
            }

            edu::LABEL_RAISED => {
                device_check(
                    "an interrupt arrived in ring 3 as a message",
                    words[0] == syscall::STATUS_OK
                        && words[1] == 1
                        && words[3] == syscall::FROM_HARDWARE,
                );
                device_check("the device agrees it was the one asking", words[2] == 1);
            }

            edu::LABEL_WORKED => {
                println!(
                    "device     : ring 3 asked for {}!, slept, and was woken with {}",
                    edu::INPUT,
                    words[2]
                );
                device_check(
                    "the device interrupted when its work was done",
                    words[0] == syscall::STATUS_OK && words[1] == 1 && words[3] == 1,
                );
                device_check(
                    "and the answer was there to read",
                    words[2] == edu::FACTORIAL as u64,
                );
            }

            other => println!("device     : unexpected label {other}"),
        }
    }

    device_check(
        "two interrupts were taken, and no more",
        irq::total(line) == 2,
    );
}

/// Compare the two ring 3 tasks' address spaces directly.
///
/// This walks the page tables rather than observing behaviour, which is the
/// point: two tasks that happen not to collide prove nothing, and arranging a
/// collision to watch it fail means racing them. Reading the tables answers the
/// actual question -- does this address mean different memory over there.
fn check_address_spaces(user: Option<usize>, service: Option<usize>) {
    let (Some(first), Some(second)) = (
        user.and_then(task::space_of),
        service.and_then(task::space_of),
    ) else {
        cap_check("each ring 3 task got its own address space", false);
        return;
    };

    cap_check(
        "each ring 3 task got its own address space",
        first.root() != second.root(),
    );

    // The same address in both, deliberately: once the spaces are separate,
    // every user task can put its stack in the same place.
    let stack_page = task::user::STACK_TOP - 4096;
    let (a, b) = (first.translate(stack_page), second.translate(stack_page));
    println!(
        "address    : user {:#x} -> {:#x} in one space, {:#x} in the other",
        stack_page,
        a.unwrap_or(0),
        b.unwrap_or(0)
    );
    cap_check(
        "the same user address is mapped in both",
        a.is_some() && b.is_some(),
    );
    cap_check("...and backed by different memory", a != b);

    // The kernel half is shared, which is what lets a trap taken in ring 3 land
    // on a mapped kernel stack whichever space happened to be installed.
    let shared = alloc::boxed::Box::new(0u64);
    let kernel_va = &*shared as *const u64 as u64;
    cap_check(
        "a kernel address resolves identically in both",
        first.translate(kernel_va) == second.translate(kernel_va)
            && first.translate(kernel_va).is_some(),
    );
}

/// A ring 3 program that reaches for kernel memory. Off by default: it ends the
/// boot, which is the proof.
#[cfg(feature = "user-test")]
#[unsafe(naked)]
extern "C" fn user_fault_program() {
    core::arch::naked_asm!(
        // A kernel address, mapped and readable -- from ring 0. The USER bit is
        // absent, so from here this must fault.
        "movabs rax, {kernel_address}",
        "mov rax, [rax]",
        "2:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 2b",
        kernel_address = sym kmain,
        yield_op = const syscall::SYS_YIELD,
        gate = const vectors::SYSCALL,
    )
}

/// Hand the ring 3 task the one capability it gets.
fn user_grants(user: Option<usize>) {
    let Some(id) = user else { return };

    cap_check(
        "grant the user task a send-only endpoint capability",
        cap::grant(id, USER_ENDPOINT, USER_TASK_ENDPOINT, cap::Rights::WRITE)
            == Ok(cap::Rights::WRITE),
    );
    // A second capability, and this one carries GRANT: something it is allowed
    // to hand on to somebody else. Narrowed from bootstrap's own Frame
    // capability, which holds every right.
    cap_check(
        "grant the user task a Frame capability it may pass on",
        cap::grant(id, 8, USER_TASK_FRAME, cap::Rights::READ_GRANT) == Ok(cap::Rights::READ_GRANT),
    );
    cap_check(
        "the user task holds exactly two capabilities",
        cap::slots_used(id) == Some(2),
    );
}

/// Serve the ring 3 task from the bootstrap task.
///
/// Three messages, in the order it sends them, so nothing here depends on
/// scheduling. The middle one is a call and gets answered.
fn serve_user() {
    for _ in 0..USER_MESSAGES {
        let received = match ipc::recv(USER_ENDPOINT, Some(USER_TRANSFER), Some(USER_REPLY)) {
            Ok(received) => received,
            Err(error) => {
                println!("user       : recv failed: {:?}", error);
                return;
            }
        };

        match received.message.label {
            LABEL_USER_DENIED => {
                println!(
                    "user       : ring 3 task reported status {} calling an empty slot",
                    received.message.words[0]
                );
                cap_check(
                    "ring 3 call through an empty slot was denied",
                    received.message.words[0] == cap::CapError::Empty.code(),
                );
            }

            LABEL_COUNT => {
                let mut buffer = [0u8; ipc::TEXT_CAPACITY];
                let length = received.message.text_bytes(&mut buffer);
                let text = core::str::from_utf8(&buffer[..length]).unwrap_or("<non-utf8>");
                println!("user       : ring 3 task called with {text:?}");

                let Some(slot) = received.reply else {
                    cap_check("ring 3 call arrived with a reply capability", false);
                    continue;
                };
                cap_check(
                    "answered the ring 3 task's call",
                    ipc::reply(
                        slot,
                        ipc::Message::from_word(LABEL_RESULT, length as u64),
                        None,
                    )
                    .is_ok(),
                );
            }

            LABEL_USER_ECHO => {
                cap_check(
                    "ring 3 task received the answer to its call",
                    received.message.words[0] == USER_PING_LEN,
                );
            }

            LABEL_USER_RIGHTS => {
                // It asked for every right while holding only WRITE. The
                // narrowing rule is the same code on both sides of the ring
                // boundary, so the answer has to be the same too.
                cap_check(
                    "ring 3 narrowing yields only what the task already held",
                    received.message.words[0] == cap::Rights::WRITE.bits() as u64,
                );
            }

            LABEL_USER_NOGRANT => {
                cap_check(
                    "ring 3 cannot pass on a capability it has no GRANT for",
                    received.message.words[0] == cap::CapError::InsufficientRights.code(),
                );
            }

            LABEL_USER_GIVE => match received.capability {
                Some((kind, rights)) => {
                    println!("user       : ring 3 task handed over a {kind:?} with {rights:?}");
                    cap_check(
                        "a capability transferred out of ring 3 arrives narrowed",
                        kind == cap::ObjectKind::Frame && rights == cap::Rights::READ,
                    );
                    cap_check(
                        "the transferred Frame names the frame bootstrap minted",
                        cap::frame_address(USER_TRANSFER)
                            == Ok(FRAME_CAP_ADDR.load(Ordering::Relaxed)),
                    );
                    let _ = cap::delete(USER_TRANSFER);
                }
                None => cap_check(
                    "a capability transferred out of ring 3 arrives narrowed",
                    false,
                ),
            },

            other => println!("user       : unexpected label {other}"),
        }
    }

    // The CPU writes CS into the trap frame on the way in, so this is the one
    // claim about privilege the measured code could not have faked.
    cap_check("syscalls arrived from ring 3", syscall::seen_cs() & 3 == 3);
}

/// Holds rw-: can print, cannot pass the console on.
extern "C" fn scribe_task() {
    if let Err(error) = cap::console_write(0, "cap        : scribe printed via its capability\n") {
        println!("cap        : scribe unexpectedly denied: {:?}", error);
    }

    cap_check(
        "scribe cannot grant onward without GRANT",
        cap::grant(0, 0, 5, cap::Rights::READ) == Err(cap::CapError::InsufficientRights),
    );
}

/// Holds r--: has a console capability and still cannot print with it.
extern "C" fn mute_task() {
    cap_check(
        "mute cannot write with an r-- capability",
        cap::console_write(0, "this must never appear\n") == Err(cap::CapError::InsufficientRights),
    );
}

/// Busy counters, one per worker task. If these all advance, every task is
/// actually being scheduled rather than one of them hogging the CPU.
static WORK: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Burn a slice of time, bump a counter, repeat.
fn work(index: usize, cooperative: bool) -> ! {
    loop {
        for _ in 0..500_000 {
            core::hint::spin_loop();
        }
        WORK[index].fetch_add(1, Ordering::Relaxed);

        // One task yields voluntarily so both paths into the scheduler get
        // exercised, not just preemption.
        if cooperative {
            task::yield_now();
        }
    }
}

extern "C" fn worker_a() {
    work(0, false)
}

extern "C" fn worker_b() {
    work(1, true)
}

/// Recurse until the stack guard page stops us. Off by default: it ends the
/// boot, which is the point.
#[cfg(feature = "stack-test")]
extern "C" fn stack_bomb() {
    fn recurse(depth: u64) -> u64 {
        // Big enough frame that this reaches the guard quickly.
        let pad = [depth; 64];
        core::hint::black_box(&pad);
        depth + recurse(depth + 1)
    }

    println!();
    println!("stack test : recursing until the guard page stops us");
    core::hint::black_box(recurse(0));
}

/// Print what each task has been doing.
fn report_tasks() {
    let mut buffer = [task::TaskInfo::default(); 16];
    let count = task::snapshot(&mut buffer);

    let finished = buffer[..count]
        .iter()
        .filter(|i| i.state == task::State::Finished)
        .count();

    print!("tasks      :");
    for info in buffer[..count]
        .iter()
        .filter(|i| i.state != task::State::Finished)
    {
        print!(
            " {}:{}{}@{} {:?} {}sw |",
            info.id,
            info.name,
            if info.user { "(r3)" } else { "" },
            info.cpu,
            info.state,
            info.switches
        );
    }
    println!(" ({finished} finished)");
    println!(
        "work       : worker={} yielder={}",
        WORK[0].load(Ordering::Relaxed),
        WORK[1].load(Ordering::Relaxed)
    );
}

/// Check every CPU that came online is actually taking its own timer
/// interrupts.
///
/// Measured against PIT channel 2, polled, exactly as [`timer_check`] is and
/// for the same reason: judging the CPUs' timers by a counter those timers
/// drive is circular, and hides the case that matters -- a core that reported
/// itself online and then went quiet. A CPU whose LAPIC timer never starts
/// looks perfectly healthy from the bootstrap processor, right up until a task
/// pinned to it never runs again.
fn smp_check(online: usize) {
    let mut before = [0u64; arch::x86_64::cpu::MAX_CPUS];
    for (cpu, slot) in before.iter_mut().enumerate().take(online) {
        *slot = lapic::ticks_on(cpu);
    }

    const WINDOW_US: u32 = 200_000;
    pit::delay_us(WINDOW_US);
    let expected = (TICK_HZ as u64 * WINDOW_US as u64) / 1_000_000;

    // Half the expected rate is the bar. The bootstrap processor is
    // preemptible by now, so the polled window can stretch, and the question
    // being asked is "is this core ticking at all", not "to what accuracy".
    let mut all_ticking = true;
    print!("smp check  : {WINDOW_US}us wall ->");
    for (cpu, was) in before.iter().enumerate().take(online) {
        let delta = lapic::ticks_on(cpu) - was;
        print!(" cpu{cpu}={delta}");
        if delta < expected / 2 {
            all_ticking = false;
        }
    }
    println!(" (expect ~{expected} each)");

    cap_check(
        "every online cpu is taking its own timer ticks",
        all_ticking,
    );
}

/// Measure both timers against a known second of wall time.
///
/// The tick counters cannot check each other -- deriving "elapsed seconds" from
/// one of them and then using that to judge the other is circular, and hides
/// exactly the case where a timer runs at the wrong rate. PIT channel 2 is
/// polled, independent of both, and its accuracy is what made LAPIC calibration
/// possible in the first place, so it is the honest reference here.
fn timer_check() {
    let lapic_before = lapic::ticks();
    let pit_before = pit::ticks();
    let clock_before = time::now_ns();

    pit::delay_us(1_000_000);

    let lapic_ticks = lapic::ticks() - lapic_before;
    let pit_ticks = pit::ticks() - pit_before;
    let clock_us = (time::now_ns() - clock_before) / 1000;

    println!(
        "timer check: 1.00s wall -> lapic {} ticks, pit via ioapic {} ticks (expect ~{})",
        lapic_ticks, pit_ticks, TICK_HZ
    );

    // The monotonic clock was calibrated over 50 ms; this is a different,
    // longer window, so it checks the calibration rather than repeating it.
    println!("clock check: 1.00s wall -> {clock_us} us by the monotonic clock");
    check(
        "clock check",
        "the monotonic clock agrees with the PIT to 1%",
        clock_us.abs_diff(1_000_000) <= 10_000,
    );
}

/// Ticks from whichever timer is actually running.
fn ticks() -> u64 {
    if lapic::timer_hz() > 0 {
        lapic::ticks()
    } else {
        pit::ticks()
    }
}

/// Wrap the firmware framebuffer in the software renderer, if we got one.
fn init_renderer() -> Option<SoftwareRenderer> {
    let fb = limine::FRAMEBUFFER.response.get()?.first()?;

    if fb.memory_model != limine::MEMORY_MODEL_RGB {
        println!("framebuffer: unsupported memory model {}", fb.memory_model);
        return None;
    }

    let info = SurfaceInfo {
        width: fb.width as u32,
        height: fb.height as u32,
        pitch: fb.pitch as u32,
        format: PixelFormat {
            bits_per_pixel: fb.bpp,
            red_mask_size: fb.red_mask_size,
            red_shift: fb.red_mask_shift,
            green_mask_size: fb.green_mask_size,
            green_shift: fb.green_mask_shift,
            blue_mask_size: fb.blue_mask_size,
            blue_shift: fb.blue_mask_shift,
        },
    };

    // SAFETY: Limine maps the framebuffer and reports its true geometry, so
    // `pitch * height` bytes from `address` are writable. Nothing else in the
    // kernel touches those pixels -- this is the only renderer we construct, and
    // it is never dropped -- so exclusive ownership holds for all of time.
    let mut renderer = match unsafe { SoftwareRenderer::new(fb.address, info) } {
        Ok(renderer) => renderer,
        Err(err) => {
            println!("framebuffer: rejected by backend: {:?}", err);
            return None;
        }
    };

    println!(
        "framebuffer: {}x{} pitch={} {}bpp",
        info.width, info.height, info.pitch, info.format.bits_per_pixel
    );

    match map_back_buffer(&info) {
        Some(back) => {
            // SAFETY: `map_back_buffer` mapped `back_buffer_len` writable
            // bytes there from frames fresh off the allocator, so they overlap
            // nothing, the framebuffer included. The mapping is never removed
            // and this renderer is the only thing given the address.
            unsafe { renderer.attach_back_buffer(back) };
            println!(
                "backbuffer : {} KiB at {:p}, the screen is only ever written",
                SoftwareRenderer::back_buffer_len(&info) / 1024,
                back
            );
        }
        // Still correct, just not pretty: a repaint is visible while it
        // happens, and blending reads video memory.
        None => println!("backbuffer : none -- drawing straight onto the screen"),
    }

    // Somewhere to keep stencils -- text, for now. Off the kernel heap, and
    // never freed: the renderer holds it for as long as there is a screen.
    let atlas = alloc::vec![0u8; (ATLAS_WIDTH * ATLAS_HEIGHT) as usize];
    renderer.attach_mask_atlas(atlas.leak(), ATLAS_WIDTH);
    println!(
        "mask atlas : {}x{}, {} KiB",
        ATLAS_WIDTH,
        ATLAS_HEIGHT,
        ATLAS_WIDTH * ATLAS_HEIGHT / 1024
    );

    Some(renderer)
}

/// The mask atlas: room for the boot screen's two lines of text.
const ATLAS_WIDTH: u32 = 1024;
const ATLAS_HEIGHT: u32 = 256;

/// Where the back buffer is mapped: a gigabyte above the heap, in the same
/// PML4 slot. That slot is populated before any user address space is forked,
/// so pages added under it now appear in every space at once.
const BACK_BUFFER_BASE: u64 = mm::heap::HEAP_START as u64 + (1 << 30);

/// Largest back buffer we will map. A 4K screen needs 32 MiB.
const BACK_BUFFER_MAX: usize = 64 << 20;

/// Frames that must be left over after the back buffer has taken its share.
/// It is a nicety; tasks and their stacks are not.
const BACK_BUFFER_RESERVE: usize = 1024;

/// Map ordinary RAM to compose frames in.
fn map_back_buffer(info: &SurfaceInfo) -> Option<*mut u8> {
    let bytes = SoftwareRenderer::back_buffer_len(info);
    if bytes == 0 || bytes > BACK_BUFFER_MAX {
        return None;
    }
    let pages = bytes.div_ceil(4096);

    // Checked up front so that running out half way, which would strand the
    // frames already taken, does not happen in practice. One table per 512
    // pages, plus the two levels above.
    let (free, _) = mm::frame::stats();
    if free < pages + pages / 512 + 2 + BACK_BUFFER_RESERVE {
        return None;
    }

    for i in 0..pages as u64 {
        let frame = mm::frame::alloc()?;
        let mapped = mm::paging::with_kernel_mapper(|mapper| {
            mapper.map_4k(
                BACK_BUFFER_BASE + i * 4096,
                frame.addr(),
                mm::paging::WRITABLE | mm::paging::NO_EXECUTE | mm::paging::GLOBAL,
            )
        });
        if mapped != Some(true) {
            return None;
        }
    }
    Some(BACK_BUFFER_BASE as *mut u8)
}

/// Side of the white corner markers.
///
/// The markers are a deliberate diagnostic -- if pitch handling is ever wrong
/// they land in the wrong place, which is obvious at a glance.
const MARKER: u32 = 16;
const BLOCK_WIDTH: u32 = 96;
const STRIP_HEIGHT: u32 = 28;

/// How solid the veil gets, out of 255.
const VEIL_OPACITY: i32 = 150;
/// How far below its resting place the veil starts and ends.
const VEIL_DROP: i32 = 28;
const VEIL_FADE_MS: u32 = 320;

/// How the block gets to its next resting place.
#[derive(Clone, Copy)]
enum Move {
    /// From wherever it is, at whatever speed it is doing.
    Spring(Spring),
    /// Over a fixed number of milliseconds.
    Ease(u32, Ease),
}

#[derive(Clone, Copy)]
enum Act {
    /// Send the block to one side.
    Block { right: bool, how: Move },
    /// Bring the veil in, or take it away.
    Veil { shown: bool },
}

struct Step {
    at_ms: u64,
    act: Act,
}

/// What the screen does, on repeat.
///
/// The two block steps 180 ms apart are deliberate: the block is sent back
/// while still in flight, which is the case a stepped or restarted animation
/// gets visibly wrong. The veil arrives and leaves while the block is moving
/// underneath it, so every one of its frames blends over something different.
const SCRIPT: [Step; 6] = [
    Step {
        at_ms: 400,
        act: Act::Block {
            right: true,
            how: Move::Spring(Spring::BOUNCY),
        },
    },
    Step {
        at_ms: 1500,
        act: Act::Veil { shown: true },
    },
    Step {
        at_ms: 2200,
        act: Act::Block {
            right: false,
            how: Move::Ease(600, Ease::STANDARD),
        },
    },
    Step {
        at_ms: 3600,
        act: Act::Block {
            right: true,
            how: Move::Spring(Spring::SMOOTH),
        },
    },
    Step {
        at_ms: 3780,
        act: Act::Block {
            right: false,
            how: Move::Spring(Spring::BOUNCY),
        },
    },
    Step {
        at_ms: 4300,
        act: Act::Veil { shown: false },
    },
];
const SCRIPT_MS: u64 = 6000;

/// Draw `text` into the next free rows of the mask atlas and say where it
/// went. `None` if it does not fit or the renderer has no atlas, in which
/// case the screen simply goes without that line.
fn upload_label(
    renderer: &mut impl Renderer,
    shelf: &mut u32,
    text: &str,
    style: Style,
) -> Option<Rect> {
    let extent = corrode_text::measure(text, style);
    if extent.width > ATLAS_WIDTH || *shelf + extent.height > ATLAS_HEIGHT {
        return None;
    }

    // The text engine draws into bytes it is handed and the renderer copies
    // them into memory it owns. Neither knows the other exists.
    let mut coverage = alloc::vec![0u8; extent.area()];
    corrode_text::draw(
        text,
        style,
        &mut Coverage::new(&mut coverage, extent.width as usize),
    );

    let at = Rect::new(0, *shelf as i32, extent.width, extent.height);
    renderer.upload_mask(at, &coverage).ok()?;
    *shelf += extent.height;
    Some(at)
}

/// How far below its resting place a line of text starts.
const TEXT_RISE: i32 = 18;

/// A line of text centred on the screen with its top at `y`, which fades in
/// and rises into place starting at `at`.
fn label_layer(screen: Rect, src: Rect, y: i32, color: Color, at: u64) -> Layer {
    let x = (screen.width as i32 - src.width as i32) / 2;
    let mut layer = Layer::mask(x, y + TEXT_RISE, src, color);
    layer.opacity = Animation::ease(at, 700, Fx::ZERO, Fx::from_int(255), Ease::Out);
    layer.y = Animation::spring(
        at,
        Fx::from_int(y + TEXT_RISE),
        Fx::from_int(y),
        Velocity::ZERO,
        Spring::SMOOTH,
    );
    layer
}

/// The boot screen: a background, the rust bar and the name above it, four
/// corner markers, a block that crosses the screen, and a translucent veil
/// that fades in over part of it and out again.
///
/// Nothing here advances per frame. Every moving quantity is a formula held by
/// a layer in the scene, and the scene works out what went stale.
struct Stage {
    scene: Scene<12>,
    block: LayerId,
    veil: LayerId,
    /// Where the veil rests when it is showing.
    veil_y: i32,
}

impl Stage {
    fn new(renderer: &mut impl Renderer, now: u64) -> Option<Self> {
        let info = &renderer.surface_info();
        let mut scene = Scene::new(info, BACKGROUND);
        let (width, height) = (info.width, info.height);
        let middle = (height / 2) as i32;
        let full_bar = (height / 8).max(1) as i32;

        // The bar opens from the middle. Its top edge and its height are two
        // springs with the same constants, so the centre holds still: the
        // spring equation is linear, and y = middle - height / 2 throughout.
        let mut bar = Layer::solid(Rect::new(0, middle, width, 0), RUST);
        let open = |from: i32, to: i32| {
            Animation::spring(
                now,
                Fx::from_int(from),
                Fx::from_int(to),
                Velocity::ZERO,
                Spring::BOUNCY,
            )
        };
        bar.height = open(0, full_bar);
        bar.y = open(middle, middle - full_bar / 2);
        scene.add(bar)?;

        let right = width.saturating_sub(MARKER) as i32;
        let bottom = height.saturating_sub(MARKER) as i32;
        for (x, y) in [(0, 0), (right, 0), (0, bottom), (right, bottom)] {
            scene.add(Layer::solid(Rect::new(x, y, MARKER, MARKER), Color::WHITE))?;
        }

        let block_y = middle + full_bar / 2 + 48;
        let block = scene.add(Layer::solid(
            Rect::new(0, block_y, BLOCK_WIDTH, STRIP_HEIGHT),
            Color::WHITE,
        ))?;

        // Over the lower half of the bar and the block's track, so it has
        // three different things underneath it, one of them moving.
        let veil_y = middle - full_bar / 4;
        let veil_bottom = block_y + STRIP_HEIGHT as i32 + 24;
        let mut veil = Layer::solid(
            Rect::new(
                (width / 3) as i32,
                veil_y + VEIL_DROP,
                width / 3,
                (veil_bottom - veil_y).max(0) as u32,
            ),
            VEIL,
        );
        veil.opacity = Animation::rest(Fx::ZERO);
        let veil = scene.add(veil)?;

        // The name, once the bar has opened, and a caption after it. Text is
        // a layer like any other: the same formulas move it and fade it.
        let mut shelf = 0;
        let screen = scene.screen();
        let title = Style::px((height / 8).clamp(24, 144)).weight(Weight::Bold);
        if let Some(src) = upload_label(renderer, &mut shelf, "CorrodeOS", title) {
            let y = middle - full_bar / 2 - src.height as i32 - 24;
            scene.add(label_layer(screen, src, y, TITLE, now + 500_000_000))?;
        }
        let caption = concat!("capability microkernel - v", env!("CARGO_PKG_VERSION"));
        if let Some(src) = upload_label(renderer, &mut shelf, caption, Style::px(20)) {
            let y = height as i32 - 96;
            scene.add(label_layer(screen, src, y, CAPTION, now + 1_100_000_000))?;
        }

        Some(Self {
            scene,
            block,
            veil,
            veil_y,
        })
    }

    /// Carry out one step of the script, starting at `at` -- the instant the
    /// script called for, not whenever this loop happened to wake up and
    /// notice.
    fn go(&mut self, step: &Step, at: u64) {
        match step.act {
            Act::Block { right, how } => {
                let travel = self.scene.screen().width.saturating_sub(BLOCK_WIDTH) as i32;
                let to = Fx::from_int(if right { travel } else { 0 });
                if let Some(block) = self.scene.layer_mut(self.block) {
                    block.x = match how {
                        Move::Spring(spring) => block.x.retarget(at, to, spring),
                        Move::Ease(ms, ease) => {
                            Animation::ease(at, ms, block.x.sample(at).value, to, ease)
                        }
                    };
                }
            }
            Act::Veil { shown } => {
                // Two formulas at once: opacity along a curve with a fixed
                // duration, position on a spring. Neither knows about the
                // other, and neither is told when the other finishes.
                let (opacity, y, ease) = if shown {
                    (VEIL_OPACITY, self.veil_y, Ease::Out)
                } else {
                    (0, self.veil_y + VEIL_DROP, Ease::In)
                };
                if let Some(veil) = self.scene.layer_mut(self.veil) {
                    veil.opacity = Animation::ease(
                        at,
                        VEIL_FADE_MS,
                        veil.opacity.sample(at).value,
                        Fx::from_int(opacity),
                        ease,
                    );
                    veil.y = veil.y.retarget(at, Fx::from_int(y), Spring::SNAPPY);
                }
            }
        }
    }
}

/// The window's size, in pixels.
const WINDOW_WIDTH: u32 = 256;
const WINDOW_HEIGHT: u32 = 192;
/// Rows at the bottom of the window that its client keeps redrawing.
const WINDOW_STRIP: u32 = 16;
/// How far in from the screen's top-right corner the window rests.
const WINDOW_MARGIN: i32 = 48;
/// How far below its resting place the window starts.
const WINDOW_RISE: i32 = 40;

/// Bootstrap's capability to the window's surface, with every right.
const BOOT_SURFACE: cap::CapPtr = 14;
/// A read-only copy of it, to check what read-only forbids.
const BOOT_SURFACE_READ: cap::CapPtr = 15;
/// The painter's one capability: the surface, to draw on and not to pass on.
const PAINTER_SURFACE: cap::CapPtr = 0;

/// The painter's clock is the timestamp counter shifted down by this much:
/// about a hundred columns a second on a 3 GHz machine.
const PAINTER_RATE: u32 = 25;
const STRIP_BRIGHT: u32 = 0x00f2_eee8;
const STRIP_DARK: u32 = 0x0040_1a08;

/// A pixel of the painter's picture the kernel looks at, and what it should
/// find there: `v = x ^ y`, as red `v`, green `v / 2`, blue `v / 8`.
const PROBE: (u32, u32) = (200, 77);
const PROBE_PIXEL: u32 = {
    let v = (PROBE.0 ^ PROBE.1) & 0xff;
    (v << 16) | ((v >> 1) << 8) | (v >> 3)
};

fn window_check(name: &str, ok: bool) {
    check("window", name, ok);
}

/// A program in **ring 3** that draws a window.
///
/// It holds one capability, to a surface, and no other way to affect
/// anything. It asks where the surface is, draws a picture into it, says so,
/// and from then on keeps a strip along the bottom moving -- writing a column
/// or two at a time and committing exactly those.
///
/// Where the strip has got to is a function of the timestamp counter, not a
/// count of times round the loop: scheduled late, it draws the columns it
/// missed in one go rather than falling behind. The window's place on the
/// screen is none of its business. The compositor springs it into view and
/// could move it anywhere without this task being scheduled at all.
#[unsafe(naked)]
extern "C" fn user_painter() {
    core::arch::naked_asm!(
        // Where are my pixels? The kernel answers; we never tell it.
        "mov rax, {map}",
        "mov rdi, {surface}",
        "int {gate}",
        "test rax, rax",
        "jnz 9f",
        "mov rbx, rdx", // base
        "mov r12, r10", // width
        "mov r13, r8",  // height
        "mov r14, r9",  // bytes from one row to the next

        // The picture: v = x ^ y, in rust.
        "xor r8d, r8d",
        "mov rdi, rbx",
        "2:",
        "xor ecx, ecx",
        "3:",
        "mov eax, ecx",
        "xor eax, r8d",
        "and eax, 0xff",
        "mov edx, eax",
        "shl edx, 16",
        "mov esi, eax",
        "shr esi, 1",
        "shl esi, 8",
        "or edx, esi",
        "shr eax, 3",
        "or edx, eax",
        "mov [rdi + rcx * 4], edx",
        "inc ecx",
        "cmp ecx, r12d",
        "jb 3b",
        "add rdi, r14",
        "inc r8d",
        "cmp r8d, r13d",
        "jb 2b",

        // All of it is new.
        "mov rax, {commit}",
        "mov rdi, {surface}",
        "xor esi, esi",
        "xor edx, edx",
        "mov r10, r12",
        "mov r8, r13",
        "int {gate}",

        // r15 counts columns drawn. Start from now.
        "rdtsc",
        "shl rdx, 32",
        "or rax, rdx",
        "shr rax, {rate}",
        "mov r15, rax",

        // How many columns should there have been by now?
        "4:",
        "rdtsc",
        "shl rdx, 32",
        "or rax, rdx",
        "shr rax, {rate}",
        "mov rbp, rax",
        "cmp r15, rbp",
        "jae 8f",
        // More than a whole strip behind: the oldest are already overdrawn.
        "mov rax, rbp",
        "sub rax, r15",
        "cmp rax, r12",
        "jbe 5f",
        "mov r15, rbp",
        "sub r15, r12",
        "5:",
        "mov r9, r15",

        // Column c is at x = c mod width, and each pass along the strip
        // alternates between the two colours.
        "6:",
        "mov rax, r15",
        "xor edx, edx",
        "div r12",
        "mov ecx, {bright}",
        "mov esi, {dark}",
        "test al, 1",
        "cmovnz ecx, esi",
        "mov rax, r13",
        "sub rax, {strip}",
        "imul rax, r14",
        "add rax, rbx",
        "lea rdi, [rax + rdx * 4]",
        "mov eax, {strip}",
        "7:",
        "mov [rdi], ecx",
        "add rdi, r14",
        "dec eax",
        "jnz 7b",
        "inc r15",
        "cmp r15, rbp",
        "jb 6b",

        // Say what changed and no more: the columns just drawn, or the
        // whole strip if they ran off its end and round.
        "mov rax, r9",
        "xor edx, edx",
        "div r12",
        "mov rsi, rdx",
        "mov r10, rbp",
        "sub r10, r9",
        "lea rax, [rsi + r10]",
        "cmp rax, r12",
        "jbe 12f",
        "xor esi, esi",
        "mov r10, r12",
        "12:",
        "mov rdx, r13",
        "sub rdx, {strip}",
        "mov r8, {strip}",
        "mov rax, {commit}",
        "mov rdi, {surface}",
        "int {gate}",

        "8:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 4b",

        // No surface, so nothing to do and no way to say so.
        "9:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 9b",

        map = const syscall::SYS_MAP,
        commit = const syscall::SYS_COMMIT,
        yield_op = const syscall::SYS_YIELD,
        gate = const vectors::SYSCALL,
        surface = const PAINTER_SURFACE,
        rate = const PAINTER_RATE,
        strip = const WINDOW_STRIP,
        bright = const STRIP_BRIGHT,
        dark = const STRIP_DARK,
    )
}

/// A window: a surface, the ring 3 task drawing in it, and -- once that task
/// has drawn something -- the layer showing it.
struct Window {
    surface: usize,
    client: usize,
    layer: Option<LayerId>,
    /// Whether a commit after the first has been looked at yet.
    seen_update: bool,
}

impl Window {
    /// Make a surface, show it to the renderer, and start a task that holds
    /// a capability to it and nothing else.
    fn open(renderer: &mut SoftwareRenderer) -> Option<Self> {
        use cap::{CapError, Rights};

        let surface = surface::create(WINDOW_WIDTH, WINDOW_HEIGHT)?;
        let (base, width, height) = surface::pixels(surface)?;
        // SAFETY: `surface::create` mapped `width * height` pixels at `base`
        // from frames fresh off the allocator, page-aligned, so they overlap
        // nothing. The mapping is never removed.
        unsafe { renderer.attach_surface(surface as u32, base, width, height) }.ok()?;

        window_check(
            "mint a Surface capability into slot 14",
            cap::mint(0, BOOT_SURFACE, cap::Object::Surface(surface), Rights::ALL).is_ok(),
        );

        // The rules, tried from this side before anybody else holds it.
        let whole = Rect::new(0, 0, width, height);
        window_check(
            "a kernel task has nowhere to map a surface",
            surface::map(BOOT_SURFACE).err() == Some(CapError::NoSpace),
        );
        window_check(
            "a read-only surface capability cannot commit",
            cap::copy(BOOT_SURFACE, BOOT_SURFACE_READ, Rights::READ) == Ok(Rights::READ)
                && surface::commit(BOOT_SURFACE_READ, whole) == Err(CapError::InsufficientRights),
        );
        window_check(
            "a Frame capability is not a surface",
            surface::commit(8, whole) == Err(CapError::WrongType),
        );
        window_check(
            "a commit outside the surface claims nothing",
            surface::commit(BOOT_SURFACE, Rect::new(width as i32, 0, 50, 50)).is_ok()
                && surface::take_redrawn(surface).is_none(),
        );
        window_check(
            "a commit is clipped to the surface",
            surface::commit(BOOT_SURFACE, Rect::new(-10, -10, 20, 20)).is_ok()
                && surface::take_redrawn(surface) == Some(Rect::new(0, 0, 10, 10)),
        );

        let client = task::spawn_user("painter", user_painter)?;
        window_check(
            "grant the painter a surface to draw on, not pass on",
            cap::grant(client, BOOT_SURFACE, PAINTER_SURFACE, Rights::READ_WRITE)
                == Ok(Rights::READ_WRITE)
                && cap::slots_used(client) == Some(1),
        );
        start_all(&[Some(client)]);

        Some(Self {
            surface,
            client,
            layer: None,
            seen_update: false,
        })
    }

    /// Take whatever the client has committed since the last look.
    ///
    /// The first commit is what puts the window on the screen: until a
    /// client has drawn something there is nothing to show, so an empty
    /// window is never seen. Every later one becomes damage to that layer.
    fn poll<const N: usize>(&mut self, scene: &mut Scene<N>, now: u64) {
        let Some(redrawn) = surface::take_redrawn(self.surface) else {
            return;
        };
        let Some((base, width, height)) = surface::pixels(self.surface) else {
            return;
        };

        if let Some(layer) = self.layer {
            if !self.seen_update {
                self.seen_update = true;
                let strip = Rect::new(0, (height - WINDOW_STRIP) as i32, width, WINDOW_STRIP);
                window_check(
                    "a later commit names only what was redrawn",
                    strip.contains(&redrawn),
                );
            }
            scene.damage(layer, redrawn);
            return;
        }

        window_check(
            "the painter's first commit is its whole surface",
            redrawn == Rect::new(0, 0, width, height),
        );
        // SAFETY: PROBE is inside the surface, whose pixels are mapped
        // readable at `base` for good. Read by value: ring 3 may be writing.
        let probe = unsafe {
            base.add((PROBE.1 * width + PROBE.0) as usize)
                .read_volatile()
        };
        window_check(
            "the kernel reads the pixels ring 3 wrote",
            probe == PROBE_PIXEL,
        );
        let theirs = task::space_of(self.client)
            .and_then(|space| space.translate(surface::user_address(self.surface)));
        window_check(
            "both are looking at the same memory",
            theirs.is_some() && theirs == surface::frame_of(self.surface, 0),
        );

        let screen = scene.screen();
        let x = screen.width as i32 - width as i32 - WINDOW_MARGIN;
        let mut layer = Layer::surface(
            x,
            WINDOW_MARGIN + WINDOW_RISE,
            self.surface as u32,
            width,
            height,
        );
        layer.opacity = Animation::ease(now, 400, Fx::ZERO, Fx::from_int(255), Ease::Out);
        layer.y = Animation::spring(
            now,
            Fx::from_int(WINDOW_MARGIN + WINDOW_RISE),
            Fx::from_int(WINDOW_MARGIN),
            Velocity::ZERO,
            Spring::SMOOTH,
        );
        self.layer = scene.add(layer);
    }
}

/// Paint what moved, then sleep until the next interrupt.
///
/// There is no animation task behind this and no frame timer. The loop wakes
/// on the scheduling tick the kernel already takes, asks the formulas where
/// things are at the current nanosecond, and writes only the pixels that
/// answer changed.
fn render_loop(renderer: &mut SoftwareRenderer) -> ! {
    let Some(mut stage) = Stage::new(renderer, time::now_ns()) else {
        println!("scene      : no room for the boot screen's layers");
        idle_loop()
    };
    let mut window = Window::open(renderer);
    if window.is_none() {
        println!("window     : none -- no memory for a surface or its task");
    }
    let screen = stage.scene.screen().area();

    // A renderer that fails paints nothing, which is what the counters below
    // would then show.
    let mut frame = |stage: &mut Stage, now| stage.scene.frame(renderer, now).unwrap_or_default();

    let start = time::now_ns();
    let first = frame(&mut stage, start);
    check(
        "scene check",
        "the first frame paints the whole screen",
        first.stale == screen,
    );
    check(
        "scene check",
        "asking again at the same instant paints nothing",
        frame(&mut stage, start) == Painted::default(),
    );
    println!();

    let mut cycle_start = start;
    let mut next_step = 0;

    let hz = TICK_HZ as u64;
    let mut next_report = 1u64;
    let (mut wakeups, mut painted, mut stale, mut drawn) = (0u64, 0u64, 0u64, 0u64);

    loop {
        let now = time::now_ns();

        loop {
            let due = cycle_start + SCRIPT[next_step].at_ms * 1_000_000;
            if now < due {
                break;
            }
            stage.go(&SCRIPT[next_step], due);
            next_step += 1;
            if next_step == SCRIPT.len() {
                next_step = 0;
                cycle_start += SCRIPT_MS * 1_000_000;
            }
        }

        // Has the window's client drawn anything? One lock and a compare;
        // the client itself is not asked and need not be running.
        if let Some(window) = window.as_mut() {
            window.poll(&mut stage.scene, now);
        }

        let result = frame(&mut stage, now);
        wakeups += 1;
        if result.stale > 0 {
            painted += 1;
            stale += result.stale;
            drawn += result.drawn;
        }

        // A few seconds of proof on serial, then stop cluttering the log.
        // Both counters are reported: the LAPIC timer is delivered by the local
        // APIC, the PIT by the I/O APIC, so if both advance then both delivery
        // paths work.
        let seconds = ticks() / hz;
        if seconds >= next_report && next_report <= 5 {
            report_timers(seconds);
            report_tasks();
            println!(
                "paint      : {} wakeups, {} painted, {} KiB shown, {} KiB drawn ({} KiB had each one repainted)",
                wakeups,
                painted,
                stale * 4 / 1024,
                drawn * 4 / 1024,
                wakeups * screen * 4 / 1024
            );
            next_report = seconds + 1;
        }

        // Nothing to do until the next tick.
        x86_64::halt();
    }
}

/// No framebuffer: still prove the timer is running.
fn idle_loop() -> ! {
    let hz = TICK_HZ as u64;
    let mut next_report = 1u64;
    loop {
        let seconds = ticks() / hz;
        if seconds >= next_report && next_report <= 5 {
            report_timers(seconds);
            next_report = seconds + 1;
        }
        x86_64::halt();
    }
}

/// Print both tick counters. Two independent sources advancing is the proof
/// that the LAPIC and the I/O APIC are each delivering.
fn report_timers(seconds: u64) {
    println!(
        "timer      : {}s -- lapic {} ticks, pit via ioapic {} ticks, {} across all cpus",
        seconds,
        lapic::ticks(),
        pit::ticks(),
        lapic::ticks_total()
    );

    // Per CPU as well as in total. A single number cannot distinguish four
    // cores ticking correctly from one core ticking four times as fast, and
    // "the other CPUs are alive" is exactly what needs proving here.
    print!("cpu ticks  :");
    for cpu in 0..smp::online() {
        print!(" cpu{}={}", cpu, lapic::ticks_on(cpu));
    }
    println!();
}

/// Deliberately take a page fault, to check the handler reports something
/// useful. Off by default -- it ends the boot.
#[cfg(feature = "fault-test")]
fn fault_test() {
    println!();
    println!("fault test : reading unmapped 0xdead0000");
    // SAFETY: none -- that is the entire point. This address is not mapped, so
    // the read is expected to trap into the #PF handler and never return.
    unsafe {
        core::ptr::read_volatile(0xdead_0000usize as *const u64);
    }
}

/// Put a ring 3 task in front of kernel memory and let it reach. The USER bit
/// is absent from every kernel mapping, so this must fault -- if it returns,
/// the ring boundary is decorative and everything above it is theatre.
///
/// Off by default: it ends the boot, which is the point.
#[cfg(feature = "user-test")]
fn user_fault_test() {
    println!();
    println!("user test  : ring 3 task reading a kernel address (should fault)");
    start_all(&[task::spawn_user("user-bomb", user_fault_program)]);
}

/// Try to write to `.text`. With W^X in force this must page-fault; if it
/// silently succeeds, the mapping is wrong and everything above is theatre.
#[cfg(feature = "wx-test")]
fn wx_test() {
    println!();
    println!("wx test    : writing to .text (should fault)");
    let target = kmain as *const () as *mut u8;
    // SAFETY: none intended -- this is expected to trap. If it returns, the
    // test has failed and we say so.
    unsafe {
        target.write_volatile(0x90);
    }
    println!("wx test    : FAILED -- .text was writable");
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Stop the other CPUs first, for the same reasons as an unhandled trap.
    lapic::broadcast(vectors::HALT);

    // SAFETY: we never return from this function, so breaking the console lock
    // cannot corrupt a print that will later resume. It is only broken if this
    // CPU holds it; one held elsewhere is waited for.
    unsafe { serial::force_unlock() };

    println!();
    println!("*** KERNEL PANIC ***");
    println!("{}", info);
    halt_forever();
}
