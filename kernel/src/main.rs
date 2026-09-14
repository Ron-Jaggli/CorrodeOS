//! Corrode kernel entry point.
//!
//! Current state: Limine hands us a framebuffer and a memory map, we bring up
//! COM1, install descriptor tables, build a physical frame allocator, start the
//! PIT, and then sit in a render loop driven by timer interrupts.
//!
//! No paging of our own, no tasks, no userspace yet.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod cap;
mod ipc;
mod limine;
mod mm;
mod serial;
mod sync;
mod syscall;
mod task;

use core::sync::atomic::{AtomicU64, Ordering};

use arch::x86_64::{self, halt_forever, ioapic, lapic, pic, pit, vectors};
use corrode_render::{
    software::SoftwareRenderer, Color, Command, CommandList, PixelFormat, Rect, Renderer,
    SurfaceInfo,
};

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

    // SAFETY: the IDT has a handler for all 256 vectors, and every source we
    // have unmasked -- LAPIC timer, the PIT via one I/O APIC line, and the
    // spurious vector -- is dispatched explicitly.
    unsafe { x86_64::enable_interrupts() };

    timer_check();

    // Scheduler last: it adopts the current context as task 0, and from the
    // next timer tick onwards this function is running under preemption.
    task::init();

    // Seed the capability system, then check its invariants while execution is
    // still single-threaded and the output is deterministic.
    cap_bootstrap();
    cap_checks();

    // Spawn and grant without preemption in between. A spawned task is Ready
    // immediately, and one scheduled before it has been given its capabilities
    // would correctly -- but confusingly -- report that it holds nothing.
    x86_64::without_interrupts(|| {
        let scribe = task::spawn("scribe", scribe_task);
        let mute = task::spawn("mute", mute_task);
        cap_grants(scribe, mute);
    });

    x86_64::without_interrupts(|| {
        let server = task::spawn("console-srv", console_server);
        let client = task::spawn("ipc-client", ipc_client);
        ipc_grants(server, client);
    });

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

    // Ring 3. Spawn and grant without preemption in between, as above.
    #[cfg(not(feature = "user-test"))]
    {
        let user = x86_64::without_interrupts(|| {
            let user = task::spawn_user("user", user_program);
            user_grants(user);
            user
        });
        cap_check("spawned a ring 3 task", user.is_some());
        serve_user();
        let service = call_user_service();
        check_address_spaces(user, service);
    }

    #[cfg(feature = "user-test")]
    user_fault_test();

    task::spawn("worker", worker_a);
    task::spawn("yielder", worker_b);
    #[cfg(feature = "stack-test")]
    task::spawn("stack-bomb", stack_bomb);
    println!("sched      : preemptive round-robin at {} Hz", TICK_HZ);

    #[cfg(feature = "fault-test")]
    fault_test();

    #[cfg(feature = "wx-test")]
    wx_test();

    match init_renderer() {
        Some(mut renderer) => {
            let info = renderer.surface_info();
            println!(
                "framebuffer: {}x{} pitch={} {}bpp",
                info.width, info.height, info.pitch, info.format.bits_per_pixel
            );
            println!();
            render_loop(&mut renderer)
        }
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

/// Bring up the APIC: parse the MADT, start the LAPIC timer, and route the PIT
/// through the I/O APIC.
///
/// Returns false if anything is missing, in which case the caller keeps the
/// PIC. Routing the PIT here as well as running the LAPIC timer is deliberate:
/// two independent counters advancing proves both the LAPIC *and* the I/O APIC
/// delivery paths, which a single timer would not.
fn init_apic() -> bool {
    let (Some(rsdp), Some(hhdm)) = (limine::RSDP.response.get(), limine::HHDM.response.get())
    else {
        println!("acpi       : no RSDP from the bootloader");
        return false;
    };

    // Limine has handed this back as a physical address in some base revisions
    // and an HHDM pointer in others, so normalise rather than assume.
    let raw = rsdp.address;
    let rsdp_phys = if raw >= hhdm.offset {
        raw - hhdm.offset
    } else {
        raw
    };

    // SAFETY: the bootloader guarantees this points at the firmware's RSDP, and
    // init_paging mapped every region the memory map reports, ACPI included.
    let Some(madt) = (unsafe { acpi::find_madt(rsdp_phys, hhdm.offset) }) else {
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
/// observed it has to say through the one capability it was given.
const LABEL_USER_DENIED: u64 = 5;
const LABEL_USER_ECHO: u64 = 6;

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

/// Report one capability invariant.
fn cap_check(name: &str, ok: bool) {
    println!(
        "cap check  : {:<52}{}",
        name,
        if ok { "PASS" } else { "FAIL" }
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
        "int {gate}",

        // A real request, and one with an answer: how long is this word?
        "mov rax, {call}",
        "mov rdi, {endpoint}",
        "mov rsi, {label_count}",
        "movabs rdx, {ping}",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
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
        "int {gate}",

        // Done talking. Stay a live ring 3 task in the rotation rather than
        // returning -- there is no ring 3 runtime to return to.
        "2:",
        "mov rax, {yield_op}",
        "int {gate}",
        "jmp 2b",

        call = const syscall::SYS_CALL,
        send = const syscall::SYS_SEND,
        yield_op = const syscall::SYS_YIELD,
        gate = const vectors::SYSCALL,
        endpoint = const USER_TASK_ENDPOINT,
        empty_slot = const USER_TASK_EMPTY,
        label_count = const LABEL_COUNT,
        label_denied = const LABEL_USER_DENIED,
        label_echo = const LABEL_USER_ECHO,
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
        // of them. r12 rather than rcx or r11 to hold the result: those two are
        // the registers fast syscalls clobber.
        "mov rax, {send}",
        "mov rdi, {endpoint}",
        "xor rsi, rsi",
        "xor rdx, rdx",
        "xor r10, r10",
        "xor r8, r8",
        "xor r9, r9",
        "int {gate}",
        "mov r12, rax",

        // Answer, carrying both the result and the status of that attempt --
        // the only way a task with no console can report on itself.
        "mov rax, {reply}",
        "mov rdi, {reply_slot}",
        "mov rsi, {label_result}",
        "lea rdx, [rbx + 1]",
        "mov r10, r12",
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
    let service = x86_64::without_interrupts(|| {
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
        service
    });

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
    cap_check(
        "the user task holds exactly one capability",
        cap::slots_used(id) == Some(1),
    );
}

/// Serve the ring 3 task from the bootstrap task.
///
/// Three messages, in the order it sends them, so nothing here depends on
/// scheduling. The middle one is a call and gets answered.
fn serve_user() {
    for _ in 0..3 {
        let received = match ipc::recv(USER_ENDPOINT, None, Some(USER_REPLY)) {
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
            " {}:{}{} {:?} {}sw |",
            info.id,
            info.name,
            if info.user { "(r3)" } else { "" },
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

    pit::delay_us(1_000_000);

    let lapic_ticks = lapic::ticks() - lapic_before;
    let pit_ticks = pit::ticks() - pit_before;

    println!(
        "timer check: 1.00s wall -> lapic {} ticks, pit via ioapic {} ticks (expect ~{})",
        lapic_ticks, pit_ticks, TICK_HZ
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
    match unsafe { SoftwareRenderer::new(fb.address, info) } {
        Ok(renderer) => Some(renderer),
        Err(err) => {
            println!("framebuffer: rejected by backend: {:?}", err);
            None
        }
    }
}

/// The static part of the frame: background, rust bar, corner markers.
///
/// Corner markers are a deliberate diagnostic -- if pitch handling is ever
/// wrong they land in the wrong place, which is obvious at a glance.
fn draw_scene(renderer: &mut impl Renderer) {
    let info = renderer.surface_info();

    let mut storage = [Command::Clear { color: BACKGROUND }; 8];
    let mut list = CommandList::new(&mut storage);

    list.push(Command::Clear { color: BACKGROUND });

    let bar_height = (info.height / 8).max(1);
    let bar_y = (info.height / 2 - bar_height / 2) as i32;
    list.push(Command::FillRect {
        rect: Rect::new(0, bar_y, info.width, bar_height),
        color: RUST,
    });

    let s = 16u32;
    for (x, y) in [
        (0i32, 0i32),
        ((info.width - s) as i32, 0),
        (0, (info.height - s) as i32),
        ((info.width - s) as i32, (info.height - s) as i32),
    ] {
        list.push(Command::FillRect {
            rect: Rect::new(x, y, s, s),
            color: Color::WHITE,
        });
    }

    let _ = renderer.submit(&list);
}

const BLOCK_WIDTH: u32 = 96;
const STRIP_HEIGHT: u32 = 28;

/// Where the sweeping block sits for a given tick count.
///
/// Ping-pongs rather than wrapping, so it reads as motion instead of a jump.
fn sweep_position(ticks: u64, travel: u32) -> u32 {
    if travel == 0 {
        return 0;
    }
    let span = travel as u64 * 2;
    let phase = ticks.wrapping_mul(4) % span;
    if phase <= travel as u64 {
        phase as u32
    } else {
        (span - phase) as u32
    }
}

/// Redraw the moving block, erasing the strip behind it first.
fn draw_heartbeat(renderer: &mut impl Renderer, x: u32, strip_y: i32) {
    let info = renderer.surface_info();

    let mut storage = [Command::Clear { color: BACKGROUND }; 2];
    let mut list = CommandList::new(&mut storage);

    list.push(Command::FillRect {
        rect: Rect::new(0, strip_y, info.width, STRIP_HEIGHT),
        color: BACKGROUND,
    });
    list.push(Command::FillRect {
        rect: Rect::new(x as i32, strip_y, BLOCK_WIDTH, STRIP_HEIGHT),
        color: Color::WHITE,
    });

    let _ = renderer.submit(&list);
}

/// Draw once, then repaint only the moving part on each timer tick.
fn render_loop(renderer: &mut impl Renderer) -> ! {
    draw_scene(renderer);

    let info = renderer.surface_info();
    let bar_height = (info.height / 8).max(1);
    let strip_y = (info.height / 2 + bar_height / 2 + 48) as i32;
    let travel = info.width.saturating_sub(BLOCK_WIDTH);

    let hz = TICK_HZ as u64;
    let mut last_x = u32::MAX;
    let mut next_report = 1u64;

    loop {
        let now = ticks();

        // A few seconds of proof on serial, then stop cluttering the log.
        // Both counters are reported: the LAPIC timer is delivered by the local
        // APIC, the PIT by the I/O APIC, so if both advance then both delivery
        // paths work.
        let seconds = now / hz;
        if seconds >= next_report && next_report <= 5 {
            report_timers(seconds);
            report_tasks();
            next_report = seconds + 1;
        }

        let x = sweep_position(now, travel);
        if x != last_x {
            draw_heartbeat(renderer, x, strip_y);
            last_x = x;
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
        "timer      : {}s -- lapic {} ticks, pit via ioapic {} ticks",
        seconds,
        lapic::ticks(),
        pit::ticks()
    );
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
    x86_64::without_interrupts(|| {
        task::spawn_user("user-bomb", user_fault_program);
    });
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
    // SAFETY: we never return from this function, so stealing the console lock
    // cannot corrupt a print that will later resume. Without this a panic while
    // holding the lock would deadlock and we would lose the message.
    unsafe { serial::force_unlock() };

    println!();
    println!("*** KERNEL PANIC ***");
    println!("{}", info);
    halt_forever();
}
