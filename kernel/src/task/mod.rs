//! Tasks and the scheduler.
//!
//! Preemptive round-robin over kernel threads. There is no userspace yet, so
//! every task runs in ring 0 and shares the kernel address space; what differs
//! between them is a stack and a saved register set.
//!
//! The whole context switch is one idea: **the interrupt frame is the saved
//! context**. `common_stub` already pushes every register on entry and pops
//! them on exit, and `iretq` reloads RSP as well as RIP. So switching tasks
//! means returning a pointer to a *different* frame from `trap_dispatch` -- the
//! epilogue that was going to restore the interrupted task instead restores the
//! next one. No separate switch routine, no second copy of the register-save
//! code to keep in sync with the first.
//!
//! A task's saved state therefore always lives on its own kernel stack, which
//! is exactly where it would be anyway.

pub mod stack;
pub mod user;

use alloc::vec::Vec;
use core::mem::size_of;

use crate::arch::x86_64::{self, cpu, cpu::MAX_CPUS, gdt, idt::TrapFrame, vectors};
use crate::cap::CSpace;
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Created, granted nothing yet, and not runnable. A task stays here until
    /// its parent calls [`start`].
    ///
    /// This is not decoration. On one CPU it was enough to spawn a task with
    /// interrupts disabled and grant it capabilities before re-enabling them;
    /// with more than one CPU that guarantees nothing, because another core can
    /// pick the task up while this one still has interrupts off. Making "may
    /// run" an explicit step is the only version of this that is actually safe.
    New,
    Ready,
    Running,
    /// Waiting on something -- today that means an IPC rendezvous. The
    /// scheduler will not pick a blocked task; whoever unblocks it is
    /// responsible for setting it back to Ready.
    Blocked,
    Finished,
}

pub struct Task {
    pub id: usize,
    pub name: &'static str,
    pub state: State,
    /// Stack pointer at which this task's `TrapFrame` sits while suspended.
    saved_rsp: u64,
    /// How many times this task has been scheduled in.
    pub switches: u64,
    /// Everything this task is allowed to touch. A task with an empty cspace
    /// can do nothing but compute and yield.
    pub cspace: CSpace,
    /// A message delivered to this task while it was blocked receiving.
    pub pending_in: Option<crate::ipc::PendingMessage>,
    /// A message this task is blocked trying to send. Cleared by whoever takes
    /// it, which is how the sender learns it was received.
    pub pending_out: Option<crate::ipc::PendingMessage>,
    /// Where a capability arriving with a message should be placed. Recorded
    /// before blocking, because the sender does the placing.
    ///
    /// Doubles as the destination for a capability arriving in a *reply*: a
    /// task is either blocked receiving or blocked in a call, never both, so
    /// one slot covers both cases.
    pub recv_slot: Option<usize>,
    /// Where the reply capability for an incoming call should be placed.
    /// Recorded before blocking in `recv`, for the same reason as `recv_slot`.
    pub reply_slot: Option<usize>,
    /// Sequence number of the call this task is blocked awaiting an answer to,
    /// or `None` if it is not in a call.
    ///
    /// A reply capability names a `(task, seq)` pair, so one kept past the call
    /// it was issued for cannot answer the next call by the same task. That is
    /// what makes a reply capability one-shot even if the holder copies it.
    pub awaiting_reply: Option<u64>,
    /// Monotonic per-task call counter. Supplies the `seq` above and is never
    /// reused, so a stale token can only ever compare unequal.
    pub call_seq: u64,
    /// Top of this task's kernel stack, or 0 if it has none of its own.
    ///
    /// This is what the TSS points at while the task runs, so a trap taken in
    /// ring 3 lands on a stack belonging to the task that took it. Kernel tasks
    /// carry it too -- they simply never trigger the privilege change that
    /// makes the CPU read it.
    pub kernel_stack_top: u64,
    /// True once this task has been placed in ring 3.
    pub user: bool,
    /// The CPU this task runs on. Assigned once, at spawn, and never changed.
    ///
    /// Pinning is what makes this scheduler safe on more than one CPU, and the
    /// reason is the kernel stack. Between storing a task's `saved_rsp` and
    /// actually leaving its stack, a CPU is still executing on it -- so a second
    /// CPU that picked the task up in that window would resume it onto a stack
    /// the first is still using. Migration needs a handshake for exactly that
    /// window. Pinning removes the window instead: one task, one CPU, so no two
    /// CPUs are ever near the same stack.
    pub cpu: usize,
    /// Whether this is a CPU's idle task, which the rotation skips.
    pub is_idle: bool,
    /// This task's address space, or `None` to run in the kernel's.
    ///
    /// Only user tasks have one. Kernel tasks share the kernel address space
    /// because they *are* the kernel -- separating them would buy nothing, as
    /// they can already reach every byte of it by construction.
    pub space: Option<crate::mm::paging::Mapper>,
}

/// Slots per capability space.
pub const CSPACE_SLOTS: usize = 32;

/// A copy of a task's status, for reporting without holding the lock.
#[derive(Debug, Clone, Copy)]
pub struct TaskInfo {
    pub id: usize,
    pub name: &'static str,
    pub state: State,
    pub switches: u64,
    pub user: bool,
    pub cpu: usize,
}

struct Scheduler {
    tasks: Vec<Task>,
    /// What each CPU is running. Indexed by [`cpu::index`].
    current: [usize; MAX_CPUS],
    /// Task run only when nothing else can be. Guarantees the scheduler always
    /// has something to return, even with every other task blocked -- without
    /// it, a fully blocked system would resume a blocked task.
    ///
    /// `None` until that CPU's idle task exists. It must not be given a
    /// placeholder id: the scheduler skips idle tasks during normal selection,
    /// so a wrong value here means a real task is permanently skipped.
    ///
    /// One per CPU, because two CPUs with nothing to do both need something to
    /// run, and it cannot be the same task -- that would be two CPUs on one
    /// stack, which is the thing this design exists to prevent.
    idle: [Option<usize>; MAX_CPUS],
    /// PML4 currently installed in CR3, so a switch between two tasks that
    /// share an address space costs nothing.
    ///
    /// Writing CR3 flushes every non-global TLB entry, so doing it on a switch
    /// that changes nothing would throw away the kernel's translations several
    /// hundred times a second for no reason.
    installed_root: [u64; MAX_CPUS],
}

static SCHEDULER: SpinLock<Option<Scheduler>> = SpinLock::new(None);

/// Adopt the currently running context as task 0.
///
/// It already has a stack and a register set -- it is running on them -- so
/// nothing needs to be built. Its `saved_rsp` gets filled in the first time it
/// is preempted.
pub fn init() {
    x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        if guard.is_some() {
            return;
        }
        let tasks = alloc::vec![Task {
            id: 0,
            name: "bootstrap",
            state: State::Running,
            cpu: 0,
            is_idle: false,
            saved_rsp: 0,
            switches: 1,
            cspace: CSpace::new(CSPACE_SLOTS),
            pending_in: None,
            pending_out: None,
            recv_slot: None,
            reply_slot: None,
            awaiting_reply: None,
            call_seq: 0,
            // Task 0 runs on the stack Limine gave us, which it neither
            // allocated nor knows the extent of.
            kernel_stack_top: 0,
            user: false,
            space: None,
        }];
        let root = crate::mm::paging::kernel_root().unwrap_or(0);
        *guard = Some(Scheduler {
            tasks,
            current: [0; MAX_CPUS],
            idle: [None; MAX_CPUS],
            installed_root: [root; MAX_CPUS],
        });
    });

    // Must come after the scheduler exists, since spawning needs it.
    make_idle(0, idle_entry);
}

/// Give `cpu` an idle task of its own.
fn make_idle(cpu_index: usize, entry: extern "C" fn()) {
    let Some(id) = spawn_on("idle", entry, cpu_index) else {
        return;
    };
    x86_64::without_interrupts(|| {
        if let Some(scheduler) = SCHEDULER.lock().as_mut() {
            scheduler.tasks[id].is_idle = true;
            scheduler.tasks[id].state = State::Ready;
            scheduler.idle[cpu_index] = Some(id);
        }
    });
}

/// Adopt the context an application processor is already running as that CPU's
/// idle task.
///
/// An AP arrives running a halt loop on a stack we gave it, which is precisely
/// what an idle task is -- so rather than spawn one and switch to it, the
/// context becomes it. The alternative is a CPU with nothing to switch *away*
/// from when its first tick lands.
pub fn adopt_as_idle(cpu_index: usize, stack_top: u64) {
    x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let Some(scheduler) = guard.as_mut() else {
            return;
        };

        let id = scheduler.tasks.len();
        scheduler.tasks.push(Task {
            id,
            name: "ap-idle",
            state: State::Running,
            cpu: cpu_index,
            is_idle: true,
            // Filled in by this CPU's first tick, exactly like task 0's.
            saved_rsp: 0,
            switches: 1,
            cspace: CSpace::new(CSPACE_SLOTS),
            pending_in: None,
            pending_out: None,
            recv_slot: None,
            reply_slot: None,
            awaiting_reply: None,
            call_seq: 0,
            kernel_stack_top: stack_top,
            user: false,
            space: None,
        });

        scheduler.current[cpu_index] = id;
        scheduler.idle[cpu_index] = Some(id);
        scheduler.installed_root[cpu_index] = crate::mm::paging::kernel_root().unwrap_or(0);
    });

    gdt::set_kernel_stack(cpu_index, stack_top);
}

/// The idle task: park the CPU until something happens.
extern "C" fn idle_entry() {
    loop {
        x86_64::halt();
    }
}

/// Create a runnable task that will start at `entry`.
///
/// `entry` may return; it lands in [`task_exit`] if it does.
pub fn spawn(name: &'static str, entry: extern "C" fn()) -> Option<usize> {
    spawn_on(name, entry, next_cpu())
}

/// Spawn onto a named CPU.
pub fn spawn_on(name: &'static str, entry: extern "C" fn(), cpu_index: usize) -> Option<usize> {
    spawn_inner(name, entry as *const () as u64, None, cpu_index)
}

/// Round-robin the CPUs that are actually running kernel code.
///
/// Only online ones: a task pinned to a CPU that never came up would never run
/// at all, which is a worse failure than an uneven split.
fn next_cpu() -> usize {
    static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let online = crate::smp::online().clamp(1, MAX_CPUS);
    NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % online
}

/// Let a task run.
///
/// Separate from [`spawn`] on purpose. A task is created holding nothing, and
/// its parent grants it capabilities before it may execute a single
/// instruction; on more than one CPU there is no way to arrange that with
/// interrupt masking, because the masking only binds the CPU doing it. Making
/// "may run" its own step is the only ordering that actually holds.
pub fn start(id: usize) -> bool {
    x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let Some(scheduler) = guard.as_mut() else {
            return false;
        };
        match scheduler.tasks.get_mut(id) {
            Some(task) if task.state == State::New => {
                task.state = State::Ready;
                true
            }
            _ => false,
        }
    })
}

/// Create a task that starts in **ring 3**.
///
/// `entry` is an ordinary kernel-image function address; [`user::create`] maps
/// its frames a second time at a user address and hands back a stack. What
/// makes the task a user task is nothing more than the selectors in its initial
/// frame -- `iretq` into a CS with RPL 3 is the privilege drop, and there is no
/// other mechanism involved.
///
/// Like every other task it starts with an **empty capability space**. A user
/// task with nothing in it cannot print, cannot send, and cannot name a single
/// object; whatever it can do, its parent granted explicitly.
pub fn spawn_user(name: &'static str, entry: extern "C" fn()) -> Option<usize> {
    let image = user::create(entry as *const () as u64)?;
    spawn_inner(name, image.entry, Some(image), next_cpu())
}

fn spawn_inner(
    name: &'static str,
    entry_rip: u64,
    image: Option<user::Image>,
    cpu_index: usize,
) -> Option<usize> {
    let stack = stack::allocate()?;
    let user = image.is_some();

    // Lay out the new task's stack exactly as an interrupt would have left it.
    //
    //   [top-8, top)                  return address -> task_exit
    //   [top-8-176, top-8)            the TrapFrame the stub will pop
    //
    // The System V ABI expects RSP % 16 == 8 on entry to a function, because a
    // `call` has just pushed a return address. Placing one here reproduces
    // that, and means a task that returns lands somewhere sane.
    let entry_rsp = stack.top - 8;

    // SAFETY: `stack.top - 8` is inside the range `stack::allocate` just
    // mapped writable, and nothing else refers to this stack yet.
    unsafe {
        (entry_rsp as *mut u64).write(task_exit as *const () as u64);
    }

    // A user task resumes onto its *user* stack and its own selectors; this
    // kernel stack is only where its trap frames land. A kernel task resumes
    // onto the stack the frame itself sits on.
    let (rsp, cs, ss) = match image {
        Some(image) => (
            image.stack_top,
            gdt::USER_CODE as u64,
            gdt::USER_DATA as u64,
        ),
        None => (entry_rsp, gdt::KERNEL_CODE as u64, gdt::KERNEL_DATA as u64),
    };

    let frame_addr = entry_rsp - size_of::<TrapFrame>() as u64;

    // SAFETY: `frame_addr` is 176 bytes below `entry_rsp`, still well inside
    // the freshly mapped stack, and exclusively ours.
    unsafe {
        (frame_addr as *mut TrapFrame).write(TrapFrame {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rbp: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            rcx: 0,
            rbx: 0,
            rax: 0,
            // Never inspected on the way out; the stub just drops these two.
            vector: 0,
            error_code: 0,
            rip: entry_rip,
            cs,
            // Bit 9 (IF) set so the task is preemptible from its first
            // instruction, bit 1 is reserved and must be 1. Note what is *not*
            // here: IOPL stays 0, so a ring 3 task executing `in` or `out`
            // takes a #GP rather than talking to a device directly.
            rflags: 0x202,
            rsp,
            ss,
        });
    }

    let id = x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let scheduler = guard.as_mut()?;
        let id = scheduler.tasks.len();
        scheduler.tasks.push(Task {
            id,
            name,
            // Not runnable until `start`.
            state: State::New,
            cpu: cpu_index.min(MAX_CPUS - 1),
            is_idle: false,
            saved_rsp: frame_addr,
            switches: 0,
            // Deliberately empty. A new task starts with no authority at all;
            // whatever it can do, its parent had to hand it explicitly.
            cspace: CSpace::new(CSPACE_SLOTS),
            pending_in: None,
            pending_out: None,
            recv_slot: None,
            reply_slot: None,
            awaiting_reply: None,
            call_seq: 0,
            kernel_stack_top: stack.top,
            user,
            space: image.map(|image| image.space),
        });
        Some(id)
    })?;

    // Logged after the scheduler lock is dropped, deliberately. Printing takes
    // the console lock, and taking it underneath the scheduler lock would make
    // this the only place in the kernel with that lock order -- the kind of
    // asymmetry that is fine until the console is served by a task and then is
    // a deadlock.
    match image {
        Some(image) => crate::println!(
            "task {}     : {:<10} cpu{} ring 3, entry {:#x}, stack {:#x}, guard {:#x}",
            id,
            name,
            cpu_index,
            image.entry,
            image.stack_top,
            image.stack_guard
        ),
        None => crate::println!(
            "task {}     : {:<10} cpu{} stack {:#x}..{:#x}, guard {:#x}",
            id,
            name,
            cpu_index,
            stack.bottom,
            stack.top,
            stack.guard
        ),
    }
    Some(id)
}

/// Where a task goes if its entry function returns.
extern "C" fn task_exit() -> ! {
    x86_64::without_interrupts(|| {
        let this_cpu = cpu::index();
        if let Some(scheduler) = SCHEDULER.lock().as_mut() {
            let current = scheduler.current[this_cpu];
            scheduler.tasks[current].state = State::Finished;
        }
    });

    // The task is finished but its stack is still in use right up until the
    // switch happens, so it cannot free anything itself. Yield until the
    // scheduler stops picking us, which it now will.
    loop {
        yield_now();
    }
}

/// Give up the rest of this time slice.
///
/// Implemented as a software interrupt so it goes through exactly the same path
/// as preemption -- one context-switch mechanism, not two.
pub fn yield_now() {
    // SAFETY: vector 0x42 has a present IDT entry whose handler reschedules.
    unsafe {
        core::arch::asm!("int {}", const vectors::YIELD, options(nomem, nostack));
    }
}

/// Pick the next task to run.
///
/// Called from interrupt context with interrupts already disabled by the gate.
/// Returns the frame to resume: the one passed in if nothing else is runnable.
pub fn on_tick(frame: *mut TrapFrame) -> *mut TrapFrame {
    let this_cpu = cpu::index();

    let mut guard = SCHEDULER.lock();
    let Some(scheduler) = guard.as_mut() else {
        return frame;
    };

    // Where this task's state now lives.
    let current = scheduler.current[this_cpu];
    scheduler.tasks[current].saved_rsp = frame as u64;
    if scheduler.tasks[current].state == State::Running {
        scheduler.tasks[current].state = State::Ready;
    }

    // Round-robin over this CPU's own tasks. `current` is considered too, at
    // the final offset, so a task with no competition keeps running.
    //
    // The `cpu` filter is load-bearing rather than an optimisation: a task
    // belongs to one CPU, and picking somebody else's would put two cores on
    // one kernel stack.
    let count = scheduler.tasks.len();
    let mut chosen = None;
    for offset in 1..=count {
        let candidate = (current + offset) % count;
        let task = &scheduler.tasks[candidate];
        // Idle tasks are the fallback below, not peers in the rotation.
        if task.is_idle || task.cpu != this_cpu {
            continue;
        }
        if task.state == State::Ready {
            chosen = Some(candidate);
            break;
        }
    }

    // Nothing runnable here: fall back to this CPU's idle task rather than
    // resuming one that is blocked or finished. Before that task exists nothing
    // can block, so staying put is safe in that window.
    let next = chosen.or(scheduler.idle[this_cpu]).unwrap_or(current);

    scheduler.current[this_cpu] = next;
    if next != current {
        scheduler.tasks[next].switches += 1;
    }
    scheduler.tasks[next].state = State::Running;

    // Tell the CPU where to put a trap frame if this task traps in ring 3.
    // Done on every switch, not once at spawn: the TSS holds one pointer and
    // it has to name whichever task is about to run. Task 0 has no stack of
    // its own, and needs none -- it never runs in ring 3, so nothing ever
    // reads this for it.
    let top = scheduler.tasks[next].kernel_stack_top;
    if top != 0 {
        gdt::set_kernel_stack(this_cpu, top);
    }

    // Install the incoming task's address space.
    //
    // Safe to do here, mid-switch, for one reason: every address space shares
    // the kernel's upper half, so this code, the stack it is running on, the
    // frame it is about to return, and the page tables themselves are all
    // mapped identically on both sides of the write. Nothing under RIP or RSP
    // moves; only the lower half does.
    let root = scheduler.tasks[next]
        .space
        .map(|space| space.root())
        .or_else(crate::mm::paging::kernel_root)
        .unwrap_or(scheduler.installed_root[this_cpu]);

    if root != scheduler.installed_root[this_cpu] && root != 0 {
        scheduler.installed_root[this_cpu] = root;
        // SAFETY: `root` is a live PML4, forked from the kernel's, so it maps
        // the whole kernel half -- including the instruction after this one.
        unsafe { crate::mm::paging::activate_root(root) };
    }

    scheduler.tasks[next].saved_rsp as *mut TrapFrame
}

/// Copy out task status without allocating or holding the lock afterwards.
///
/// Takes a caller-provided buffer deliberately: allocating here would take the
/// heap lock while holding the scheduler lock, and the scheduler lock is shared
/// with an interrupt handler.
pub fn snapshot(buffer: &mut [TaskInfo]) -> usize {
    x86_64::without_interrupts(|| {
        let guard = SCHEDULER.lock();
        let Some(scheduler) = guard.as_ref() else {
            return 0;
        };
        let count = scheduler.tasks.len().min(buffer.len());
        for (slot, task) in buffer[..count].iter_mut().zip(&scheduler.tasks) {
            *slot = TaskInfo {
                id: task.id,
                name: task.name,
                state: task.state,
                switches: task.switches,
                user: task.user,
                cpu: task.cpu,
            };
        }
        count
    })
}

impl Default for TaskInfo {
    fn default() -> Self {
        Self {
            id: 0,
            name: "",
            state: State::Finished,
            switches: 0,
            user: false,
            cpu: 0,
        }
    }
}

/// Id of the task currently on this CPU.
///
/// This is not ambient authority: it identifies whose capability space is
/// "mine", the way a hand is not a permission. Every actual authority decision
/// is made from the contents of that space, never from this number.
pub fn current_id() -> Option<usize> {
    x86_64::without_interrupts(|| Some(SCHEDULER.lock().as_ref()?.current[cpu::index()]))
}

/// The CPU a task is pinned to.
pub fn cpu_of(id: usize) -> Option<usize> {
    x86_64::without_interrupts(|| Some(SCHEDULER.lock().as_ref()?.tasks.get(id)?.cpu))
}

/// The address space a task runs in, if it has one of its own.
pub fn space_of(id: usize) -> Option<crate::mm::paging::Mapper> {
    x86_64::without_interrupts(|| SCHEDULER.lock().as_ref()?.tasks.get(id)?.space)
}

/// Operate on one task's capability space.
///
/// Runs with interrupts disabled: the scheduler lock is shared with the timer
/// handler, so holding it with interrupts on risks deadlocking against our own
/// preemption.
pub fn with_cspace<R>(id: usize, f: impl FnOnce(&mut CSpace) -> R) -> Option<R> {
    x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let scheduler = guard.as_mut()?;
        let task = scheduler.tasks.get_mut(id)?;
        Some(f(&mut task.cspace))
    })
}

/// Operate on one task's control block.
///
/// Same interrupt discipline as [`with_cspace`]: the scheduler lock is shared
/// with the timer handler.
pub fn with_task<R>(id: usize, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
    x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let scheduler = guard.as_mut()?;
        let task = scheduler.tasks.get_mut(id)?;
        Some(f(task))
    })
}
