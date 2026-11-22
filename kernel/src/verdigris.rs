//! Verdigris's part of the kernel: Windows programs on CorrodeOS.
//!
//! What is here is what only Verdigris needs. The boot checks that a PE
//! loads and runs (phase 1), binds its imports to kernel32 (phase 2) and
//! reads files through a view (phase 3); starting a Windows program with
//! its view (`start_windows`); `window::RUN`, which runs one from the files
//! for a holder of the runner's endpoint (`run_windows`); and the watcher
//! that says how it ended. Loading a PE stays in `task::user`, beside the
//! ELF loader it shares its image building with. See
//! `verdigris/README.md` and `verdigris/DESIGN.md`.

use super::*;

/// Whom a program was run for, as the log says it. Information, not
/// authority: holding the runner's endpoint is what lets a task run one.
fn run_for(task: usize) -> &'static str {
    match task::name_of(task) {
        Some("terminal") => "the terminal",
        Some("files") => "Files",
        Some(name) => name,
        None => "a task that has gone",
    }
}

/// `window::RUN`, on the runner's endpoint: a program given the files --
/// the terminal, Files -- runs a Windows program from them. Its view is
/// its own folder alone; its output goes to an endpoint the caller is
/// handed, to receive on; a watcher ends that output when the program
/// ends, however it ends.
pub(crate) fn run_windows(from: usize, text: &str) -> ([u64; 4], Option<(cap::CapPtr, cap::Rights)>) {
    const FILE_NOT_FOUND: u64 = 2;
    const BAD_EXE_FORMAT: u64 = 193;
    let refuse = |code: u64| ([0, code, 0, 0], None);

    let caller = run_for(from);
    if vault::locked() {
        return refuse(FILE_NOT_FOUND);
    }
    let Some((tree, module)) = vault::root() else {
        return refuse(FILE_NOT_FOUND);
    };
    let (path, arguments) = text.split_once('\n').unwrap_or((text, ""));
    let Ok(fs) = corrode_shell::fs::Fs::from_tar(module.bytes) else {
        return refuse(FILE_NOT_FOUND);
    };
    let Some(exe) = fs.get(path).filter(|node| !node.is_dir()) else {
        return refuse(FILE_NOT_FOUND);
    };
    let folder = corrode_shell::fs::parent_of(path).unwrap_or("/");
    // A task's name lives as long as the task: the file's name, cut short.
    let name: alloc::string::String = corrode_shell::fs::name_of(path).chars().take(15).collect();
    let name: &'static str = alloc::boxed::Box::leak(name.into_boxed_str());
    // As Windows would show it: the program where its view puts it, then
    // the arguments as the terminal quoted them.
    let file = corrode_shell::fs::name_of(path);
    let mut command_line = if file.contains(' ') { alloc::format!("\"C:\\{file}\"") } else { alloc::format!("C:\\{file}") };
    if !arguments.is_empty() {
        command_line.push(' ');
        command_line.push_str(arguments);
    }

    // Spawning reaps too; done here first to say what came back.
    let reaped = task::reap();
    println!("desktop    : {reaped} frames of finished tasks given back");
    let output = cap::issue(SERVICE_SPARE_SLOT, cap::Object::Endpoint(ipc::Endpoint::new()), cap::Rights::ALL);
    let slots = Slots {
        view: SERVICE_VIEW_SLOT,
        page: SERVICE_PAGE_SLOT,
        output: SERVICE_SPARE_SLOT,
        log: SERVICE_LOG_SLOT,
    };
    let started = output
        .map_err(|_| "no endpoint for its output")
        .and_then(|_| start_windows(name, exe.data, &command_line, folder, &[], tree, &slots));
    let (program, server) = match started {
        Ok(started) => started,
        Err(why) => {
            println!("desktop    : {path} was not started: {why}");
            return refuse(BAD_EXE_FORMAT);
        }
    };
    let watcher = task::spawn("win-watch", watch_program);
    let watching = watcher.is_some_and(|id| {
        cap::grant(id, SERVICE_SPARE_SLOT, WATCH_OUTPUT, cap::Rights::WRITE).is_ok()
            && cap::grant(id, SERVICE_VIEW_SLOT, WATCH_VIEW, cap::Rights::WRITE).is_ok()
    });
    if let (true, Some(id)) = (watching, watcher) {
        arch::x86_64::without_interrupts(|| WATCHES.lock().push(Watch { watcher: id, program, server }));
    }
    // What the run has cost so far: with the earlier runs' tasks given back
    // as this one was set up, this stays level from run to run. Taken before
    // its tasks start, so nothing they take is in it; and said in one print
    // with the line before, so nothing any other task prints comes between.
    let (objects, _) = memory::stats();
    println!(
        "desktop    : {path} started for {caller}, seeing {folder} alone\n\
         desktop    : {} frames free, {objects} memory objects",
        mm::frame::stats().0
    );
    start_all(&[Some(server), Some(program), watcher.filter(|_| watching)]);
    // Received on, not sent to, and not passed on.
    ([1, 0, 0, 0], Some((SERVICE_SPARE_SLOT, cap::Rights::READ)))
}

/// A program the terminal ran, and the task watching it.
struct Watch {
    watcher: usize,
    program: usize,
    server: usize,
}

static WATCHES: SpinLock<alloc::vec::Vec<Watch>> = SpinLock::new(alloc::vec::Vec::new());
/// A watcher's capabilities: the program's output and its view, send-only.
const WATCH_OUTPUT: cap::CapPtr = 0;
const WATCH_VIEW: cap::CapPtr = 1;
/// How long the terminal is shown a program's output.
const RUN_WAIT_NS: u64 = 10_000_000_000;

/// Wait for one program the terminal ran to end, then say so on its
/// output, after its last line. One still running at the deadline is
/// stopped (`task::stop`). A program that faulted or was stopped never told
/// its view to end; this does.
extern "C" fn watch_program() {
    use corrode_abi::{log, view};

    let me = task::current_id().unwrap_or(usize::MAX);
    let watch = arch::x86_64::without_interrupts(|| {
        let mut watches = WATCHES.lock();
        let at = watches.iter().position(|watch| watch.watcher == me)?;
        Some(watches.swap_remove(at))
    });
    let Some(watch) = watch else {
        return;
    };
    let finished = |id| task::state_of(id) == Some(task::State::Finished);
    let deadline = time::now_ns() + RUN_WAIT_NS;
    while !finished(watch.program) && time::now_ns() < deadline {
        task::sleep_until(time::now_ns() + 20_000_000);
    }
    // Still running at the deadline: stopped, if it is in its own code.
    let mut stopped = false;
    if !finished(watch.program) && task::stop(watch.program) {
        let grace = time::now_ns() + 1_000_000_000;
        while !finished(watch.program) && time::now_ns() < grace {
            task::sleep_until(time::now_ns() + 10_000_000);
        }
        stopped = finished(watch.program);
    }
    let ended = finished(watch.program);
    if ended {
        // Given a moment: a program that exited told its view itself.
        let grace = time::now_ns() + 1_000_000_000;
        while !finished(watch.server) && time::now_ns() < grace {
            task::sleep_until(time::now_ns() + 10_000_000);
        }
        if !finished(watch.server) {
            let _ = ipc::send(WATCH_VIEW, ipc::Message { label: view::DONE, words: [0; 4] }, None);
        }
    }
    let (how, detail) = match (ended, task::ending_of(watch.program)) {
        _ if stopped => (log::END_STOPPED, 0),
        (true, Some(task::Ending::Exited(code))) => (log::END_ENDED, u64::from(code)),
        (true, Some(task::Ending::Faulted(vector))) => (log::END_FAULTED, u64::from(vector)),
        (true, _) => (log::END_STOPPED, 0),
        (false, _) => (log::END_RUNNING, 0),
    };
    let _ = ipc::send(WATCH_OUTPUT, ipc::Message { label: log::END, words: [how, detail, 0, 0] }, None);
}

fn verdigris_check(name: &str, ok: bool) {
    check("verdigris", name, ok);
}

/// Where a Windows program finds the log, in its own capability space: the
/// only capability `hello.exe` holds.
const PE_LOG: cap::CapPtr = 0;

/// Verdigris, phase 1 (see `verdigris/DESIGN.md`): a Windows program loads
/// from a module and runs as an ordinary ring 3 task.
///
/// `hello.exe` is a PE file written byte by byte by `verdigris/scripts/hello-exe.sh`.
/// The host tests feed the PE reader files that lie; these show that the
/// real one maps as its sections ask, that a damaged copy is refused before
/// anything is mapped, and that it runs: it says one line, through the
/// log endpoint it is given and nothing else, and exits.
pub(crate) fn checks() {
    use mm::paging::{NO_EXECUTE, USER, WRITABLE};

    verdigris_check(
        "a file that is not a PE is refused",
        matches!(
            task::user::load_pe(b"MZ, but nothing else a PE needs"),
            Err(task::user::LoadError::Pe(_))
        ),
    );
    let Some(module) = modules::find("hello.exe") else {
        println!("verdigris  : no hello.exe module was loaded");
        return;
    };
    let allowed = corrode_abi::layout::IMAGE_BASE..corrode_abi::layout::IMAGE_END;
    let Ok(pe) = corrode_pe::Pe::parse(module.bytes, allowed) else {
        verdigris_check("hello.exe is a PE this kernel runs", false);
        return;
    };

    // The same file with its code section made writable as well: refused
    // whole, before a frame is taken.
    let mut damaged = alloc::vec::Vec::from(module.bytes);
    let flags = 0x58 + 240 + 36;
    if let Some(field) = damaged.get_mut(flags..flags + 4) {
        let value = u32::from_le_bytes([field[0], field[1], field[2], field[3]]) | 0x8000_0000;
        field.copy_from_slice(&value.to_le_bytes());
    }
    verdigris_check(
        "a writable and executable section is refused",
        matches!(
            task::user::load_pe(&damaged),
            Err(task::user::LoadError::Pe(corrode_pe::Error::WritableAndExecutable))
        ),
    );

    let hello = match task::spawn_pe("hello.exe", module.bytes) {
        Ok(id) => id,
        Err(error) => {
            println!("verdigris  : hello.exe was not loaded: {error:?}");
            verdigris_check("load a PE from a module", false);
            return;
        }
    };
    let honoured = task::space_of(hello).is_some_and(|space| {
        pe.segments().all(|segment| {
            segment.pages().step_by(4096).all(|page| {
                space.leaf_flags(page).is_some_and(|flags| {
                    flags & USER != 0
                        && (flags & WRITABLE != 0) == segment.writable
                        && (flags & NO_EXECUTE == 0) == segment.executable
                })
            })
        })
    });
    verdigris_check("a PE's headers and sections map as it asks", honoured);

    let granted = cap::grant(hello, LOG_ENDPOINT, PE_LOG, cap::Rights::WRITE).is_ok();
    verdigris_check(
        "it holds the log, and nothing else",
        granted && cap::slots_used(hello) == Some(1),
    );
    start_all(&[Some(hello)]);
    // It says its line and exits; bounded, in case it never does.
    let deadline = time::now_ns() + 2_000_000_000;
    while task::state_of(hello) != Some(task::State::Finished) && time::now_ns() < deadline {
        task::yield_now();
    }
    verdigris_check(
        "it runs and exits",
        task::state_of(hello) == Some(task::State::Finished),
    );

    console_checks();
}

/// Verdigris, phase 2: `console.exe` (written by `verdigris/scripts/console-exe.sh`)
/// makes no system call of its own. It imports GetStdHandle, WriteFile and
/// ExitProcess from KERNEL32.dll, and the kernel binds them to Verdigris's
/// kernel32, mapped into the same task, before a page of it is mapped.
fn console_checks() {
    use mm::paging::{NO_EXECUTE, USER, WRITABLE};

    let (Some(program), Some(kernel32)) = (modules::find("console.exe"), modules::find("kernel32"))
    else {
        println!("verdigris  : no console.exe or kernel32 module was loaded");
        return;
    };

    // The same program asking for a function kernel32 does not have:
    // "GetStdHandle" as "GetStdHandlX". Refused whole.
    let mut damaged = alloc::vec::Vec::from(program.bytes);
    if let Some(byte) = damaged.get_mut(0x400 + 0x92 + 11) {
        *byte = b'X';
    }
    verdigris_check(
        "an import kernel32 does not have is refused",
        matches!(
            task::user::load_windows(&damaged, kernel32.bytes),
            Err(task::user::LoadError::Pe(corrode_pe::Error::Unresolved))
        ),
    );

    let console = match task::spawn_windows("console.exe", program.bytes, kernel32.bytes) {
        Ok(id) => id,
        Err(error) => {
            println!("verdigris  : console.exe was not loaded: {error:?}");
            verdigris_check("load a Windows program with kernel32", false);
            return;
        }
    };
    let base = corrode_abi::verdigris::KERNEL32_BASE;
    let (Ok(pe), Ok(runtime)) = (
        corrode_pe::Pe::parse(program.bytes, corrode_abi::layout::IMAGE_BASE..base),
        corrode_elf::Elf::parse(kernel32.bytes, base..corrode_abi::layout::IMAGE_END),
    ) else {
        return;
    };
    let Some(space) = task::space_of(console) else {
        return;
    };

    // Every import's slot, read back through the task's own page tables,
    // holds the address kernel32's table gives for it.
    let exports = runtime
        .segments()
        .find(|segment| segment.address == base)
        .and_then(|segment| corrode_pe::exports::Exports::parse(segment.data));
    let hhdm = limine::HHDM.response.get().map(|response| response.offset);
    let bound = exports.zip(hhdm).is_some_and(|(exports, hhdm)| {
        let mut all = true;
        pe.each_import(|import| {
            all &= {
                let expected = exports.find(import.dll, import.name);
                let held = space.translate(import.slot).map(|physical| {
                    // SAFETY: the frame is the task's, mapped by the direct
                    // map; the slot is 8-aligned, so inside one page.
                    unsafe { ((physical + hhdm) as *const u64).read_volatile() }
                });
                expected.is_some() && held == expected
            };
        });
        all && pe.import_count() == 3
    });
    verdigris_check("its imports are bound into kernel32's code", bound);

    let honoured = pe.segments().chain(runtime.segments()).all(|segment| {
        segment.pages().step_by(4096).all(|page| {
            space.leaf_flags(page).is_some_and(|flags| {
                flags & USER != 0
                    && (flags & WRITABLE != 0) == segment.writable
                    && (flags & NO_EXECUTE == 0) == segment.executable
            })
        })
    });
    verdigris_check("it and kernel32 map as their files ask", honoured);

    let granted = cap::grant(console, LOG_ENDPOINT, corrode_abi::verdigris::LOG, cap::Rights::WRITE).is_ok();
    verdigris_check(
        "it too holds the log, and nothing else",
        granted && cap::slots_used(console) == Some(1),
    );
    start_all(&[Some(console)]);
    let deadline = time::now_ns() + 2_000_000_000;
    while task::state_of(console) != Some(task::State::Finished) && time::now_ns() < deadline {
        task::yield_now();
    }
    verdigris_check(
        "it writes through kernel32 and exits",
        task::state_of(console) == Some(task::State::Finished),
    );

    view_checks();
}

/// Bootstrap's capabilities for reader.exe's view: its endpoint and the
/// page the program and the view share. Both are granted on, narrowed.
const VIEW_ENDPOINT: cap::CapPtr = 76;
const VIEW_PAGE: cap::CapPtr = 77;

/// What reader.exe may see: its own folder, and one file elsewhere. Its
/// `C:\` is then `/`, where only `docs` and `etc` show, and in `etc` only
/// `motd`.
const READER_FOLDER: &str = "/docs";
const READER_ALLOWED: &str = "/etc/motd";

/// Verdigris, phase 3: `reader.exe` (assembled from `verdigris/scripts/reader-exe.S`)
/// reads files with CreateFileW, ReadFile, GetFileSize and CloseHandle,
/// through a view of them (`verdigris/DESIGN.md` section 4).
///
/// The view is `fileview`, a task of its own holding the files and the
/// list of what the program may see. The program holds neither: its view
/// endpoint, send-only, and a page shared with the view for paths and
/// what is read. What it may read and what is not there for it is in the
/// log; `pe-test` reads it.
fn view_checks() {
    let Some(program) = modules::find("reader.exe") else {
        println!("verdigris  : no reader.exe module was loaded");
        return;
    };
    let Some((tree, _)) = vault::root() else {
        println!("verdigris  : no files on the boot medium (a vault keeps them); reader.exe is not run");
        return;
    };
    let slots = Slots { view: VIEW_ENDPOINT, page: VIEW_PAGE, output: LOG_ENDPOINT, log: LOG_ENDPOINT };
    let started = start_windows("reader.exe", program.bytes, "reader.exe", READER_FOLDER, &[READER_ALLOWED], tree, &slots);
    let (reader, server) = match started {
        Ok(started) => started,
        Err(why) => {
            println!("verdigris  : reader.exe and its view were not started: {why}");
            verdigris_check("load a Windows program and its view", false);
            return;
        }
    };
    verdigris_check(
        "the view holds the files, its list, page and log",
        cap::slots_used(server) == Some(6),
    );
    verdigris_check(
        "reader.exe holds its view, page and heap, not the files",
        cap::slots_used(reader) == Some(4),
    );
    start_all(&[Some(server), Some(reader)]);
    let finished = |id| task::state_of(id) == Some(task::State::Finished);
    let deadline = time::now_ns() + 5_000_000_000;
    while !(finished(reader) && finished(server)) && time::now_ns() < deadline {
        task::yield_now();
    }
    verdigris_check(
        "it reads through its view; the view ends with it",
        finished(reader) && finished(server),
    );
}

/// Runs its closure when dropped: for what must happen on every way out.
struct Defer<F: FnMut()>(F);

impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Where in the caller's own capability space `start_windows` puts the
/// view's endpoint and page while it hands them out (the caller deletes
/// them after), and the endpoint, held with GRANT, where the program's
/// output goes.
struct Slots {
    view: cap::CapPtr,
    page: cap::CapPtr,
    output: cap::CapPtr,
    /// The log, for the view's own lines, held with GRANT.
    log: cap::CapPtr,
}

/// Make a Windows program and its view (`fileview`), not started: the
/// program at `exe`, seeing `folder` and `allowed` of the files in module
/// `tree`. The program holds its output endpoint, its view send-only and
/// the page they share; the view holds the files, the list, its endpoint,
/// the page, the log and a pool. Nothing else.
fn start_windows(
    name: &'static str,
    exe: &[u8],
    command_line: &str,
    folder: &str,
    allowed: &[&str],
    tree: usize,
    slots: &Slots,
) -> Result<(usize, usize), &'static str> {
    use corrode_abi::{verdigris, view};

    let (Some(kernel32), Some(service)) = (modules::find("kernel32"), modules::find("fileview")) else {
        return Err("no kernel32 or fileview module");
    };
    let (Some(page), Some(list)) = (memory::create(1, false), memory::create(1, false)) else {
        return Err("no memory for the view");
    };
    // Neither is the kernel's to keep: each goes when the program and its
    // view no longer hold or map it, and the capabilities below are made
    // first, so neither can go before.
    let _let_go = Defer(|| {
        memory::let_go(page);
        memory::let_go(list);
    });
    // The command line, in UTF-16 at the start of the page, for kernel32 to
    // take before the page is used for anything else. The page is zeroed,
    // so it ends in a zero: at most 2047 units are written.
    let Some(shared) = memory::frame_of(page, 0) else {
        return Err("no memory for the view");
    };
    let units: alloc::vec::Vec<u8> = command_line.encode_utf16().take(2047).flat_map(u16::to_le_bytes).collect();
    // SAFETY: the object's frame, which no task has been given yet; at most
    // 4094 bytes from its start.
    mm::paging::with_kernel_mapper(|mapper| unsafe { mapper.write_frame(shared, 0, &units) });
    // The list, written through the direct map before the view maps it,
    // read-only. One page: what does not fit is left out.
    let Some(frame) = memory::frame_of(list, 0) else {
        return Err("no memory for the view");
    };
    let mut offset = 0;
    for part in core::iter::once(folder).chain(allowed.iter().copied()) {
        for bytes in [part.as_bytes(), b"\n"] {
            let bytes = &bytes[..bytes.len().min(4096 - offset)];
            // SAFETY: the object's frame, which no task has been given yet;
            // `offset + bytes.len()` stays within the page.
            mm::paging::with_kernel_mapper(|mapper| unsafe { mapper.write_frame(frame, offset, bytes) });
            offset += bytes.len();
        }
    }
    let program = task::spawn_windows(name, exe, kernel32.bytes).map_err(|_| "not a program Verdigris runs")?;
    let server = task::spawn_program("fileview", service.bytes).map_err(|_| "the view would not load")?;

    let made = cap::issue(slots.view, cap::Object::Endpoint(ipc::Endpoint::new()), cap::Rights::ALL).is_ok()
        && cap::issue(slots.page, cap::Object::Memory(memory::hold(page)), cap::Rights::ALL).is_ok();
    let served = made
        && cap::grant(server, slots.view, view::SERVE, cap::Rights::READ).is_ok()
        && cap::grant(server, slots.page, view::PAGE, cap::Rights::READ_WRITE).is_ok()
        && cap::grant(server, slots.log, view::LOG, cap::Rights::WRITE).is_ok()
        && cap::issue_to(server, view::TREE, cap::Object::Blob(tree), cap::Rights::READ).is_ok()
        && cap::issue_to(server, view::SPEC, cap::Object::Memory(memory::hold(list)), cap::Rights::READ).is_ok()
        && cap::issue_to(
            server,
            view::POOL,
            cap::Object::Pool(core::sync::atomic::AtomicU64::new(view::HEAP_PAGES as u64)),
            cap::Rights::READ_WRITE,
        )
        .is_ok();
    let given = made
        && cap::grant(program, slots.output, verdigris::LOG, cap::Rights::WRITE).is_ok()
        && cap::grant(program, slots.view, verdigris::VIEW, cap::Rights::WRITE).is_ok()
        && cap::grant(program, slots.page, verdigris::PAGE, cap::Rights::READ_WRITE).is_ok()
        // A heap to grow into, as a native program has: a budget of pages,
        // none taken until kernel32 asks.
        && cap::issue_to(
            program,
            verdigris::POOL,
            cap::Object::Pool(core::sync::atomic::AtomicU64::new(verdigris::POOL_PAGES)),
            cap::Rights::READ_WRITE,
        )
        .is_ok();
    if !(served && given) {
        return Err("its capabilities could not be handed out");
    }
    Ok((program, server))
}
