# Threads for Verdigris: a design

Design only: nothing here is built yet. It says how a Windows program on
CorrodeOS gets a second thread, what that costs, in what order to build it,
and which boot tests would prove each step. It answers DESIGN.md's
"Threads" entry ("`CreateThread` needs the kernel to start a second task in
an existing address space"), which stays open until the first step lands.

## 1. What the kernel has today

From `kernel/src/task/mod.rs`, `task/user.rs`, `memory.rs` and
`mm/paging.rs`:

- **A task is the unit of everything.** `Task` holds its own register
  state (`saved_rsp`, a boxed `FpuState`), its own kernel stack
  (`stack: Option<Stack>`, its top in `kernel_stack_top` for the TSS), its
  own capability space (`cspace: CSpace`), its own IPC state
  (`pending_in`, `pending_out`, `recv_slot`, `reply_slot`,
  `awaiting_reply`, `call_seq`), its own sleep deadline (`wake_at`), its
  own GS base (`gs_base`, written on every switch), and its own address
  space (`space: Option<Mapper>`).
- **An address space belongs to one task.** `Mapper` is `Copy` (a PML4's
  physical address and the direct-map offset) and owns nothing; ownership
  is by convention. `reap` takes a finished task's `space` and calls
  `memory::forget_space` and `release_user_half` on it. Two tasks holding
  the same `Mapper` today would have it torn down when the first one is
  reaped, under the other.
- **A task is pinned to one CPU**, at spawn, for life (`cpu`). The comment
  gives the reason: a task's kernel stack is still in use for a moment after
  it is switched out, and pinning removes the window in which a second CPU
  could resume it.
- **There is no TLB shootdown.** `memory.rs` says it: "taking a kernel
  mapping away needs every CPU to forget it, and nothing here can make
  another CPU do that yet". `invlpg` is only ever local.
- **Memory outlives its capabilities while it is mapped.** A memory
  object goes back to the allocator only when no capability names it
  (`Hold`) *and* no address space maps it (`mapped`, decremented by
  `forget_space`). So dropping a task's capabilities never frees memory
  that is still mapped in a space another task uses. That property is what
  makes threads possible without a shared capability space (section 3).
- **Ring 3 already gets memory without fixed addresses**: `SYS_ALLOC` buys
  pages from a pool capability and `SYS_MAP` maps them at an address the
  kernel picks (`USER_BASE` plus a gigabyte per object). The heap grows
  this way.
- **Blocking**: a task can block on IPC (`SYS_RECV`, `SYS_CALL`), on the
  clock (`SYS_SLEEP`), or on a window (`SYS_WAIT`). Nothing lets a task
  block on a word of memory, which every lock and wait between threads
  needs.
- **Stopping**: `stop(id)` marks one task; its CPU finishes it at the next
  tick that finds it in ring 3. `finish_current` ends the running task.
  Nothing ends a group of tasks together.
- **The Windows layout** (`abi::verdigris`, `task/user.rs`): the image
  from 0x40_0000, kernel32 at 0x0800_0000, the TEB (2 pages) at
  0x0f00_0000, the PEB after it with the start words at +0x800 and the TLS
  vector at +0x900, the TLS block a page after the PEB, the stack's guard
  page at 0x1000_0000 (`IMAGE_END`) and 32 pages of stack above it.

## 2. The model: a program is a group of tasks

A thread is a task. Every per-task field in section 1 is exactly what a
thread needs of its own: registers, FPU state, kernel stack, IPC state,
sleep deadline, GS base. What threads of one program share is the address
space, and with it everything in memory.

The kernel gains one record, a **program**: the address space, a count of
the tasks in it that have not finished, the program's exit code, and the
ids of its tasks. A task's `space: Option<Mapper>` becomes a reference to
its program's record (an index or an `Arc`). Nothing else in `Task`
changes.

- `reap` releases a space only when the last task of its program has left
  (`left`, as today, for every one of them). That is the only change to
  teardown, and it is the riskiest line in this design: get it wrong and a
  page table is freed under a running thread.
- **Ending.** When any thread calls `ExitProcess`, or faults, the program
  ends: every task in it is stopped (`stop`, as today, for each), and the
  exit code is the one given (or the fault's). When a thread returns or
  calls `ExitThread`, only it ends; when the last thread ends, the program
  ends with that thread's code, as on Windows. The watcher in
  `kernel/src/verdigris.rs` waits on the program, not on its first task.
- **One CPU per program, for now.** Every thread of a program runs on the
  CPU its first task was given. With no shootdown, a thread on another CPU
  could keep using a translation another thread had just removed. Verdigris
  removes nothing while a program runs, but a program can make system
  calls itself, so the kernel cannot rely on that: same-CPU is the rule
  the kernel enforces, not a convention. The cost is plain: threads give a
  program concurrency (one blocks, another runs) but no parallelism. Section
  6's last step lifts it.

## 3. Capabilities: copied at creation

Each task keeps its own `CSpace`. A new thread's `CSpace` is filled, at
creation, with copies of what the creating thread holds in the Verdigris
slots: the log, the view, the shared page, the pool and the heap pieces,
at the same slot numbers. Copies name the same objects (`Arc`s), so the
pool's budget, the view's endpoint and the page are shared, which is what
a program expects.

Why not one shared `CSpace` behind a lock: the IPC paths take
`&mut task.cspace` in many places and place capabilities into a
receiver's slots while holding the sender's; with two threads of one
program sharing a `CSpace`, a call from one to the other would take the
same lock twice. Copying needs no change to IPC at all.

What copying costs, honestly:

- A capability one thread receives after creation is not seen by the
  others. Verdigris programs receive none after start today. When window
  support arrives (a window capability per window, made at run time), this
  decision comes back: the shared `CSpace` may then be worth its locking.
- A heap piece bought by thread B lands in B's `CSpace`. When B ends and
  its capabilities are dropped, the memory stays, because it is still
  mapped in the program's space (section 1). It is given back with the
  space. Nothing leaks past the program.
- `kernel32` must not hand a slot number from one thread to another as if
  it were the same capability. It does not today; it must not start.

## 4. Where each thread's stack and TEB go

The first thread keeps today's layout, unchanged, so single-threaded
programs and every existing test stay as they are.

Each further thread gets a **slot** of 1 MiB from 0x1100_0000 (above the
first thread's stack, below anything else), up to 64 slots, so up to 65
threads in a program:

```
slot n at 0x1100_0000 + n * 0x10_0000
  +0x0_0000  never mapped: the guard below everything in the slot
  +0x0_1000  stack: the top `size` bytes up to +0xE_0000 are mapped,
             everything below them is not, so overflow faults
  +0xE_0000  the thread's TEB, 2 pages, as the first thread's
  +0xE_2000  its TLS vector (one page)
  +0xE_3000  its TLS block, up to 64 KiB (corrode-pe's MAX_TLS)
  +0xF_3000  free, never mapped
```

- **Stack size**: `CreateThread`'s size, rounded up to pages; 0 means the
  program's default (the PE header's `SizeOfStackCommit`, at least
  64 KiB). The most is 892 KiB (0xE_0000 minus the guard). A larger
  request fails with `ERROR_NOT_ENOUGH_MEMORY`, rather than giving less
  than was asked and letting the thread fault later.
- **Paid for from the program's pool.** The kernel takes the slot's pages
  from the program's pool capability (`verdigris::POOL`), as `SYS_ALLOC`
  does. A program cannot make the kernel spend memory it was not given; a
  thread it cannot pay for is refused.
- **The TEB is written by the kernel before the thread runs**, as the
  first one is: `StackBase`, `StackLimit`, `Self`, the TLS vector at +0x58,
  the PEB at +0x60, and the thread's id at +0x48 (`ClientId`), so
  `GetCurrentThreadId` is a read of `gs:[0x48]`.
- **The TLS block** is a fresh copy of the image's template, made by the
  kernel the way the first thread's is (the template's address and length
  join the start words in the PEB, so kernel32 can also find them).
- **Fixed slots, not `SYS_MAP`'s addresses**, so a thread's TEB, stack
  and guard are where a test can look for them, as the first thread's are,
  and the guard is real: `SYS_MAP` places objects a gigabyte apart, but
  with nothing guaranteed unmapped right below a stack.

## 5. The kernel's new calls

Two, in `abi`, each taking the program's own capabilities only:

- **`SYS_THREAD`** (start a thread): the entry address, one argument, the
  stack size. It needs WRITE on the pool capability, and it finds a free
  slot, maps it as in section 4, copies the capabilities as in section 3,
  and starts a task in the caller's program, on the caller's CPU, with
  `rip` at the entry, `rcx` the argument (Windows' first argument
  register), `rsp` 8 below the stack's top (as if called), and its GS base
  at the slot's TEB. Returns the thread's id. The entry is kernel32's
  thread start, never the program's function directly (below).
- **`SYS_FUTEX_WAIT` and `SYS_FUTEX_WAKE`** (wait on a word): "sleep if the
  32-bit word at this address still holds this value, until woken or until
  this deadline", and "wake up to this many tasks waiting on this word".
  The key is (the program, the address), so only threads of one program
  can wake each other, and a program can never touch another's waiters.
  Checking the word and going to sleep happen under the scheduler's lock,
  so a wake between them is never lost. Every lock and wait in kernel32 is
  built on this pair, and nothing else in the kernel needs to know what a
  semaphore is.

The kernel also sets a word when a thread ends: the thread's TEB holds an
"ended" word, which the kernel writes to 1 and wakes before the thread's
task finishes. Waiting for a thread is then waiting on that word.

## 6. kernel32

- **`CreateThread`** asks `SYS_THREAD` to start kernel32's thread start
  with a small record (the program's function and its argument). The
  thread start runs the image's TLS callbacks with `DLL_THREAD_ATTACH`,
  calls the function as `extern "win64" fn(*mut c_void) -> u32`, and ends
  the thread with what it returns (`ExitThread`). The handle is a thread
  handle (a new range, beside files, searches and semaphores) naming the
  thread's id and slot. `_beginthreadex` in msvcrt is the same with
  msvcrt's start-up of the thread's runtime state.
- **`ExitThread`** runs the TLS callbacks with `DLL_THREAD_DETACH`, then the
  kernel ends the task (a new exit flavour of `SYS_EXIT` that ends only the
  thread). `ExitProcess`, as today, ends the program, now all of it.
- **`WaitForSingleObject` on a thread** waits on its "ended" word (with a
  deadline, for the timeout); `GetExitCodeThread` reads its code from the
  TEB. `WaitForMultipleObjects` is the same loop over several words, woken
  by any (a futex wake on each).
- **Per-thread state moves into the TEB**, where Windows keeps it:
  - `GetLastError`'s number: TEB +0x68 (`LastErrorValue`), read as
    `gs:[0x68]`; today it is a process-wide `AtomicU32`.
  - TLS slots: TEB +0x1480 (`TlsSlots`, 64 of them), so `TlsGetValue(i)` is
    `gs:[0x1480 + 8i]`; today one array (`TLS_VALUES`) for the process. The
    slots' *allocation* (`TLS_USED`) stays per process, under a lock.
  - msvcrt's `errno` and `rand`'s seed: per thread on Windows too (msvcrt
    keeps them in a per-thread block). They go in the TEB's free space.
- **Everything else shared gets a lock.** Every `One<T>` in kernel32 is
  "safe because there is one thread". With threads, each becomes one of:
  - *Read-only after start*: `ARGV`, the command line, `MSVCRT_ACMDLN` and
    the other variables, `STARTED`. Stay as they are.
  - *Per thread*: `ERRNO`, `SEED`, `LAST_ERROR`, `TLS_VALUES`, as above.
  - *Locked*: the heap (`corrode_rt`'s heap flag becomes a futex mutex
    rather than a reentrancy check), the pending output line (`PENDING`, so
    two threads' lines never interleave mid-line), the shared page and the
    view calls that use it (one call at a time, since the page holds one
    request), `AT_EXIT`, `IOB` and the FILE table, the semaphore table,
    `TLS_USED`, the heap's piece count (`PIECES`, `LAST_PIECE`),
    `UNHANDLED_FILTER`, `SIGNALS`.
  One mutex type, built on the futex pair (an atomic word: 0 free, 1 held,
  2 held with waiters; the usual three-state lock), replaces `One<T>` for
  the locked ones, and `One<T>`'s `unsafe impl Sync` is then gone.
- **Critical sections become real**: the Windows structure's
  `LockCount`/`RecursionCount`/`OwningThread` fields, kept as Windows lays
  them out (programs and mingw's runtime read them), with the owning
  thread's id from `gs:[0x48]` and the futex mutex underneath. Recursion
  by the owning thread is counted, as Windows does.
- **Semaphores become real**: the count is a word in kernel32's table;
  `ReleaseSemaphore` adds and wakes, `WaitForSingleObject` takes one or
  waits on the word with the deadline. mingw's emulated TLS (`__thread`,
  section 99 of PROGRESS) then works across threads, because its lock is
  made of these, and its per-thread pointers come from `TlsGetValue`, now
  per thread.

## 7. The order to build it, and the tests that prove each step

Each step leaves every existing test passing on both GPUs, and adds its
own.

1. **The program record** (kernel only, no new call). A space is released
   when its last task leaves. Test: a kernel boot check that starts a
   ring 3 test program (Rust, `user/`) and a second task in the same space
   through an internal call, lets the first end, checks the second still
   runs and reads memory the first wrote, then ends it and checks the
   memory objects in use return to their count (as win-test's "keeps no
   memory" does).
2. **`SYS_THREAD`** and same-CPU placement. Test: a Rust ring 3 test
   program starts three threads that each write their id into a shared
   array and spin until told to stop; the boot check reads the array, the
   tasks' CPUs (all one), and their GS bases (each its slot's TEB). A
   fourth thread asking for more stack than a slot holds is refused; a
   program with an empty pool cannot start one.
3. **Futexes.** Test: two threads ping-pong a counter 10 000 times through
   wait and wake (it ends at 10 000, every wake found its waiter); a wait
   whose word has already changed returns at once; a wait with a deadline
   returns at it; a wake on a word nobody waits on does nothing; a program
   stopped while one of its threads waits ends whole, and its memory comes
   back. A second program waking the first's address wakes nothing.
4. **Program ending.** Test: a thread calling `ExitProcess` ends every
   thread and gives its code; a thread faulting ends the program and the
   terminal says "fault" as today; the last thread returning ends the
   program with its code.
5. **kernel32 and msvcrt.** Per-thread state in the TEB, the locks,
   `CreateThread`/`ExitThread`/`_beginthreadex`, waits on threads, real
   critical sections and semaphores. Test: first, every existing win-test
   and the C corpus unchanged (the single-thread path must not move).
   Then a mingw-built `threads.c` test program, scored in win-test: eight
   threads add to one counter 100 000 times each under a critical section
   (it reaches 800 000); a `__thread` variable differs per thread;
   `TlsSetValue` in one thread is not seen in another; `GetLastError` is
   per thread; `WaitForSingleObject` on each thread returns, and
   `GetExitCodeThread` gives what each returned; a semaphore handed
   between two threads 1000 times; `printf` from all eight never splits a
   line.
6. **More than one CPU** (later, its own design): an IPI that makes other
   CPUs forget a mapping, then letting a program's threads run on several
   CPUs. Until then step 2's rule stands. This is the step that gives
   parallelism, and the one most likely to find races.

## 8. Cost and risk

- **Size.** Steps 1-4 are kernel work, perhaps 600 to 900 lines with
  their checks; step 5 is about as much again in kernel32. It is the
  largest change Verdigris has asked of the kernel.
- **The teardown is the danger.** A space freed with a thread still in it
  is a use-after-free in the kernel, and memory reused by another program
  that a thread can still read. Step 1 exists alone so this is proved
  before anything depends on it.
- **The single-thread path must not slow down or change.** Every lock in
  kernel32 is uncontended in a program with one thread: a compare-exchange
  that succeeds, no system call.
- **No parallelism until step 6**, and step 6 needs shootdown, which the
  whole kernel (not just Verdigris) has been without.
- **Copied capabilities** (section 3) are a decision that windows may
  reverse.
- **Thread-heavy programs** (thread pools of dozens, `std::thread` in
  Rust programs built for Windows) are bounded by 64 slots per program and
  by the pool's budget.

## Choices

- **Choice:** a thread is a task; a program is a group of tasks sharing a
  space, released when the last one leaves.
- **Choice:** each thread keeps its own capability space, filled with
  copies at creation, rather than one shared and locked. IPC is untouched;
  revisit when capabilities arrive at run time (windows).
- **Choice:** all of a program's threads run on its first task's CPU until
  the kernel can shoot down TLB entries. Concurrency now, parallelism
  later.
- **Choice:** fixed 1 MiB slots from 0x1100_0000, 64 of them, holding a
  guarded stack (up to 892 KiB), the TEB, the TLS vector and TLS block;
  the first thread's layout is unchanged.
- **Choice:** threads are paid for from the program's pool, so a program
  cannot spend the kernel's memory.
- **Choice:** one waiting primitive in the kernel, a futex keyed by
  (program, address); semaphores, critical sections, thread waits and
  kernel32's own locks are built on it in ring 3.
- **Choice:** per-thread state lives where Windows keeps it, in the TEB
  (`LastErrorValue` at +0x68, `TlsSlots` at +0x1480, the thread id at
  +0x48), so code that reads those fields directly works.
- **Choice:** a stack request larger than a slot holds fails, rather than
  being quietly shortened.
