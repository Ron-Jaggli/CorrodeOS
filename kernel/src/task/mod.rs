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

use alloc::vec::Vec;
use core::mem::size_of;

use crate::arch::x86_64::{self, gdt, idt::TrapFrame, vectors};
use crate::cap::CSpace;
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
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
}

/// Slots per capability space.
pub const CSPACE_SLOTS: usize = 16;

/// A copy of a task's status, for reporting without holding the lock.
#[derive(Debug, Clone, Copy)]
pub struct TaskInfo {
    pub id: usize,
    pub name: &'static str,
    pub state: State,
    pub switches: u64,
}

struct Scheduler {
    tasks: Vec<Task>,
    current: usize,
    /// Task run only when nothing else can be. Guarantees the scheduler always
    /// has something to return, even with every other task blocked -- without
    /// it, a fully blocked system would resume a blocked task.
    ///
    /// `None` until the idle task exists. It must not be given a placeholder
    /// id: the scheduler skips the idle task during normal selection, so a
    /// wrong value here means a real task is permanently skipped.
    idle: Option<usize>,
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
            saved_rsp: 0,
            switches: 1,
            cspace: CSpace::new(CSPACE_SLOTS),
            pending_in: None,
            pending_out: None,
            recv_slot: None,
            reply_slot: None,
            awaiting_reply: None,
            call_seq: 0,
        }];
        *guard = Some(Scheduler {
            tasks,
            current: 0,
            idle: None,
        });
    });

    // Must come after the scheduler exists, since spawning needs it.
    if let Some(id) = spawn("idle", idle_entry) {
        x86_64::without_interrupts(|| {
            if let Some(scheduler) = SCHEDULER.lock().as_mut() {
                scheduler.idle = Some(id);
            }
        });
    }
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
    let stack = stack::allocate()?;

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
            rip: entry as *const () as u64,
            cs: gdt::KERNEL_CODE as u64,
            // Bit 9 (IF) set so the task is preemptible from its first
            // instruction, bit 1 is reserved and must be 1.
            rflags: 0x202,
            rsp: entry_rsp,
            ss: gdt::KERNEL_DATA as u64,
        });
    }

    let id = x86_64::without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let scheduler = guard.as_mut()?;
        let id = scheduler.tasks.len();
        scheduler.tasks.push(Task {
            id,
            name,
            state: State::Ready,
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
        });
        Some(id)
    })?;

    // Logged after the scheduler lock is dropped, deliberately. Printing takes
    // the console lock, and taking it underneath the scheduler lock would make
    // this the only place in the kernel with that lock order -- the kind of
    // asymmetry that is fine until the console is served by a task and then is
    // a deadlock.
    crate::println!(
        "task {}     : {:<10} stack {:#x}..{:#x}, guard {:#x}",
        id,
        name,
        stack.bottom,
        stack.top,
        stack.guard
    );
    Some(id)
}

/// Where a task goes if its entry function returns.
extern "C" fn task_exit() -> ! {
    x86_64::without_interrupts(|| {
        if let Some(scheduler) = SCHEDULER.lock().as_mut() {
            let current = scheduler.current;
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
    let mut guard = SCHEDULER.lock();
    let Some(scheduler) = guard.as_mut() else {
        return frame;
    };

    // Where this task's state now lives.
    let current = scheduler.current;
    scheduler.tasks[current].saved_rsp = frame as u64;
    if scheduler.tasks[current].state == State::Running {
        scheduler.tasks[current].state = State::Ready;
    }

    // Round-robin over everything except idle. `current` is considered too, at
    // the final offset, so a task with no competition keeps running.
    let count = scheduler.tasks.len();
    let idle = scheduler.idle;
    let mut chosen = None;
    for offset in 1..=count {
        let candidate = (current + offset) % count;
        // Skip idle here; it is the fallback below, not a peer in the rotation.
        // Only skip it once we actually know which task it is.
        if Some(candidate) == idle {
            continue;
        }
        if scheduler.tasks[candidate].state == State::Ready {
            chosen = Some(candidate);
            break;
        }
    }

    // Nothing runnable: fall back to idle rather than resuming a task that is
    // blocked or finished. Before the idle task exists nothing can block, so
    // staying on the current task is safe in that window.
    let next = chosen.or(idle).unwrap_or(current);

    scheduler.current = next;
    if next != current {
        scheduler.tasks[next].switches += 1;
    }
    scheduler.tasks[next].state = State::Running;
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
        }
    }
}

/// Id of the task currently on this CPU.
///
/// This is not ambient authority: it identifies whose capability space is
/// "mine", the way a hand is not a permission. Every actual authority decision
/// is made from the contents of that space, never from this number.
pub fn current_id() -> Option<usize> {
    x86_64::without_interrupts(|| Some(SCHEDULER.lock().as_ref()?.current))
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
