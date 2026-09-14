# Corrode

A capability-based microkernel for x86_64, written from scratch in Rust.

Two goals: speed and security. A small privileged kernel (memory, threads,
interrupts, capabilities, IPC) and everything else — drivers, the GPU stack, the
compositor — in userspace. A crashing GPU driver should take down a process, not
the machine.

## Corrode never runs on the host

It builds in a container and boots in QEMU. That's the only place it runs.

The container gets exactly one host mount — this directory — and one host device,
`/dev/kvm`, for CPU virtualisation. No GPU passthrough, no VFIO, no USB, no host
block devices, no host network. QEMU's only disk is the ISO we just built, and
its network is `-net none`.

Don't flash this to a USB stick. Don't dual-boot it.

## Build and run

Everything goes through `./corrode`, which is a thin wrapper around
`podman run … make`. Build the image once:

```sh
podman build -t corrode-dev docker/
```

Then:

```sh
./corrode iso         # build the bootable ISO
./corrode run         # boot it in QEMU, serial on stdout (Ctrl-A X to quit)
./corrode boot-test   # headless boot, serial captured to build/serial.log
./corrode run-gui     # boot with a live framebuffer over VNC on 127.0.0.1:5900
./corrode screenshot  # boot and dump the framebuffer to build/frame.png
./corrode fault-test  # boot into a deliberate page fault
./corrode wx-test     # boot and prove .text is not writable
./corrode user-test   # boot and prove ring 3 cannot read kernel memory
./corrode check       # cargo check
./corrode shell       # drop into the build container
```

Env knobs: `ENGINE=docker`, `NO_KVM=1` to force TCG, `NET=1` to give the
container network access (off by default — there are no crates.io dependencies,
so the build genuinely doesn't need it).

## Layout

```
kernel/          the privileged kernel
  src/limine.rs    hand-rolled Limine boot protocol bindings
  src/serial.rs    16550 UART on COM1 — the console until there's better
  src/sync.rs      SpinLock — interrupts are live, so this is load-bearing
  src/acpi.rs      just enough ACPI to find the interrupt controllers
  src/arch/        GDT, IDT, LAPIC, I/O APIC, PIC, PIT, port I/O, MSRs
  src/mm/frame.rs  physical frame allocator
  src/mm/paging.rs four-level page tables, W^X
  src/mm/heap.rs   kernel heap — coalescing free list
  src/task/        tasks and the preemptive scheduler
  src/cap/         capabilities — objects, rights, capability spaces
  src/ipc.rs       synchronous IPC — endpoints, call/reply, reply capabilities
  src/syscall.rs   the ring 3 boundary — capability slots in, no pointers
  src/task/user.rs user address space — aliased code pages, user stacks
  build.rs         makes cargo notice linker.ld changes
  linker.ld        higher-half layout at -2GB, page-aligned sections
render/          the rendering interface
  src/lib.rs       Renderer trait, POD command lists
  src/software.rs  software rasteriser (the reference backend)
boot/limine.conf lets Limine find the kernel
docker/          the build environment
scripts/         check-image.sh — guards against orphan sections
```

## Design rules

**Capabilities, not identity.** No root, no ambient authority. A task can act on
an object only if it holds a capability for it, and rights only ever narrow when
a capability is passed on. A syscall that asks "who is the caller" instead of
"what does the caller hold" is a bug. If a feature seems to need ambient
authority, that's a design smell worth raising before it's built.

**`unsafe` is a budget.** Every `unsafe` block carries a `// SAFETY:` comment
stating the invariant it relies on and why it holds. If that comment can't be
written honestly, the code is wrong. Push `unsafe` down into small auditable
primitives (`arch::x86_64::port`) and keep the layers above it safe.

**The GPU sits behind a trait.** `Renderer` has a working software backend. A
hardware backend is an optimisation, never a requirement — nothing may be written
that only compiles or only boots on a machine with a particular GPU.

**Stay bootable.** Prefer increments that keep it booting over rewrites that
leave it broken for days. Anything touching boot, paging or interrupts gets
built *and* booted in QEMU before it's called done.

## On the rendering engine

The `Renderer` interface is shaped like Metal in the places Metal got it right,
and deliberately unlike it in one:

- Command lists are **plain data and own no device state**. Recording borrows
  nothing from the renderer, so N threads record N lists with no synchronisation
  and no per-queue serialisation. Metal ties a command buffer to a queue at
  creation and to ordering by creation order; here ordering is stated explicitly
  at submit, which is the only place it matters.
- Commands are **POD** — no pointers, no vtables, no lifetimes. A command list
  can be memcpy'd, sent over IPC to a userspace driver task, replayed, or lowered
  to a different backend. That's what makes a userspace GPU driver able to sit
  behind the same trait as the software rasteriser.
- **No allocation.** Storage is caller-provided, so this works today in a kernel
  with no heap and unchanged later in userspace.

### On NVIDIA, honestly

Turing and newer route everything through signed GSP firmware. There is no public
ISA documentation. Nouveau took well over a decade to get partway. The GSP-RM
path is real, but it is a multi-year subproject, not a checkbox — planning should
not assume it's close.

## Status

Boots under Limine on BIOS and UEFI. Serial console on COM1. Own GDT + TSS with
IST1 for double faults, and a 48-vector IDT built on naked stubs that normalise
every trap into one `TrapFrame`. PIC remapped to 0x20, PIT ticking at 100 Hz,
interrupts live. Physical frame allocator over the Limine memory map, with the
free list stitched through the free frames themselves. Our own four-level page
tables, replacing Limine's. Software renderer paints the framebuffer and
repaints on each tick.

A kernel heap at PML4 slot 320, so `alloc` types (`Box`, `Vec`, `String`) work.
The MADT is parsed for the interrupt controllers; the local APIC timer drives a
100 Hz tick and the I/O APIC routes device lines, with the 8259s masked off.

A preemptive round-robin scheduler over kernel threads, each with its own
guard-paged stack.

Capabilities: objects, rights, and a per-task capability space.

Synchronous IPC over endpoints, with call/reply and one-shot reply
capabilities. A console server holds the only writable console capability and
answers clients that hold nothing but a send-only endpoint.

Userspace: tasks that run in ring 3, on their own user stacks, reaching the
kernel only through one syscall gate that takes capability slots and no
pointers. `./corrode user-test` proves a ring 3 task cannot read kernel memory.

Not yet: SMP, per-task address spaces, user-side servers.

### W^X

The kernel image is mapped a page at a time with one permission set per
section: `.text` is read+execute, `.rodata` is read-only+NX, `.data`/`.bss` are
read+write+NX. Nothing is ever both writable and executable.

This only works because three things line up, and all three are easy to get
silently wrong:

- the linker script pads each section to a 4 KiB boundary, so permissions have
  somewhere to change;
- `EFER.NXE` is on, or bit 63 of a page table entry is reserved rather than
  meaningful;
- `CR0.WP` is on, or ring 0 ignores read-only pages entirely and the whole
  thing is decorative.

`./corrode wx-test` writes to `.text` and asserts it traps.

### The heap

A first-fit free list, sorted by address and **coalesced on free**. The sorting
is what makes coalescing possible — adjacent blocks can only be recognised as
adjacent if they are neighbours in the list too.

The usual toy version skips coalescing. That works in a demo and then fragments
without bound in anything long-running: free a thousand 16-byte blocks and you
can no longer satisfy a 32-byte request despite having the space. Merging on
insert costs one comparison and removes the failure mode entirely.

Allocated blocks carry no header — `dealloc` gets the `Layout` back, so the size
is recomputed rather than stored, and per-allocation overhead is zero bytes.

### Orphan sections

`kernel/linker.ld` names `.got`, `.got.plt` and `.data.rel.ro` explicitly. A
section the script does not mention becomes an *orphan*: lld places it after
everything, past `__kernel_end`, outside the range `init_paging()` maps. It
links fine and then page-faults at runtime nowhere near the cause.

`scripts/check-image.sh` runs on every build and fails if any allocatable
section lands outside `[__kernel_start, __kernel_end)`.

Relatedly, `kernel/build.rs` exists solely to tell cargo that `linker.ld` is an
input. Without it, editing the linker script does not trigger a relink and you
debug a stale binary.

### Interrupt controllers

The local APIC timer is the scheduling tick. It has no fixed frequency — it
counts at the bus clock over a programmable divisor — so it is calibrated at
boot against PIT channel 2, which is polled and needs no interrupts of its own.
On QEMU that measures ~62.5 MHz, i.e. 1 GHz / 16, which is a useful sanity check
on the calibration.

The PIT is *also* kept running and routed through the I/O APIC. That is
deliberate redundancy for testing: two counters fed by two different delivery
paths, so `timer check` can show that the local APIC and the I/O APIC are each
working. A single timer could not distinguish "delivery works" from "this one
timer works".

Routing honours the MADT's interrupt source overrides. QEMU remaps ISA IRQ0 onto
GSI 2, and ignoring that is the classic reason a correctly programmed timer
never fires.

The 8259s are masked once the APIC is up, but they are still *remapped* first: a
masked PIC can emit a spurious IRQ7, and on its power-on vector that arrives
looking like a double fault.

### Capabilities

A task can act on an object only if it holds a capability for it. No root, no
uid, no "is this the kernel asking". Every operation answers *what does the
caller hold*, never *who is the caller*.

A capability is an unforgeable reference plus a rights set. Tasks never see the
reference — they name capabilities by slot index into their own capability
space, so a task cannot even describe an object it was not given. A newly
spawned task's cspace is empty: whatever it can do, its parent handed over
explicitly.

**Rights only ever narrow.** `Rights` has no `union`, `insert`, or `with`
operation. There is no code path anywhere that adds a right to a capability, so
amplification is not prevented by a check that someone could forget to write —
it is unrepresentable. Derivation takes a mask and intersects it; asking for
more than you hold gets you less, and you are told what you actually received.

`GRANT` is the right that governs handing a capability to *another* task. A task
can hold `rw-` and use it freely while being structurally unable to spread it.

Printing is the deliberate example. In most kernels console output is ambient
authority — any code can print. Here a task needs a Console capability with
WRITE, and a task with an empty cspace has no way to produce output at all. The
kernel's own boot log still writes straight to the serial port, because the
kernel is not a protection domain; tasks are.

`cap::mint` creates authority over an object from nothing and is the one place
that can. It should have no callers outside boot-time seeding. Every capability
over an object descends from those mints by copying and narrowing.

`cap::reply_capability` is the only other constructor, and it is deliberately
not a second `mint`: it confers no authority over any object, only the right to
unblock one specific caller once. See the IPC section below.

Known gap: dropping the last capability to a `Frame` does not return it to the
allocator. Reclaiming safely means knowing nothing still maps it, which needs
mapping metadata this kernel does not keep — recorded rather than papered over
with a `Drop` that would be wrong in general.

### IPC

A send and a receive meet at an endpoint. Whichever arrives first blocks until
the other shows up, and the message is copied straight between the two tasks'
control blocks. The kernel never buffers a message and never allocates one, so
there is no queue to size, no queue to exhaust, and no way for one task to spend
another's kernel memory by sending to a peer that is not listening.

Endpoints are objects, so the two directions are separate rights: `WRITE` means
*may send*, `READ` means *may receive*. Handing a client a send-only capability
and the server a receive-only one on the same endpoint is what stops a client
impersonating the server by listening on its own endpoint. Sending a capability
inside a message requires `GRANT` on the capability being sent — the same rule
as handing one over directly, because that is exactly what it is.

#### Reply capabilities

A one-way send tells the sender its message was taken and nothing more. A
service has to answer, and how it answers is the whole security question: the
server must be able to reply to *the client that asked*, without being handed a
general capability to talk to that client whenever it likes.

`call` sends and then blocks. The kernel hands the receiver a **reply
capability** naming that one parked caller, and `reply` spends it. It is
one-shot twice over, by two independent mechanisms:

- it is deleted from the server's capability space on a successful reply;
- it carries the call's sequence number, so a copy kept behind goes stale the
  moment the caller is released, and the same task's *next* call gets a
  different number.

The second is what matters. Consuming the slot alone would be defeated by
`cap::copy`, which needs no `GRANT` because copying within your own space
normally grants nothing new. Here it would — the right to interrupt a client
long after the exchange it was trusted with — so the token, not the slot, is
what carries the expiry. A boot check copies a reply capability, spends the
original, and confirms the copy is dead.

Reply capabilities are issued with `WRITE` and no `GRANT`: the right to answer
your client is not yours to pass to someone else.

Being answered needs no authority a client did not already have, so `call`
requires exactly what `send` does. A client is already blocked; the reply is the
other half of a send it was entitled to make.

#### Refusing a call

A receiver names the slot its reply capability should land in. Passing none
declares a server that does one-way work only, and a call that reaches it is
*refused*: the caller gets `CapError::Refused` rather than parking forever, and
the sends queued behind it are still served. The same refusal happens if the
nominated slot is full — the delivery is declined whole rather than half-made,
so a caller is never blocked on an answer that structurally cannot come.

Only the caller is told. A server is never woken by a request it has already
declared it does not handle.

The caller distinguishes "answered" from "refused" with no extra state:
`reply` clears the caller's `awaiting_reply` and installs the message in one
uninterruptible step, so finding that flag cleared with nothing delivered can
only mean the other outcome.

Known gap: a caller blocked in `call` waits forever if the server simply never
replies. The honest fixes are a timeout or a fault-handler capability and
neither exists yet. Note what it is not — the caller picked that server by
holding a capability to its endpoint, so it is a service you trusted failing
you, not something any task can do to any other.

### Userspace

A ring 3 task is not a different kind of object. It is a task whose initial
trap frame names user selectors: `iretq` into a CS with RPL 3 is the privilege
drop, and there is no other mechanism involved. It starts with an **empty
capability space**, like every other task, which for a user task is the whole
point — it cannot print, cannot send, and cannot name a single object until its
parent hands something over.

Three things have to line up, and each is easy to get quietly wrong:

- every user page carries the USER bit, and the CPU ANDs that bit down the
  entire walk — so `descend` propagates it to interior entries. Widening an
  interior entry is safe in the other direction: a kernel leaf underneath still
  has to carry USER itself, and does not;
- the TSS `rsp0` is updated on **every context switch**, not once at spawn.
  There is one TSS and one pointer in it, and it has to name whichever task is
  about to run, or a trap from ring 3 lands on somebody else's stack;
- IOPL stays 0 in the initial rflags, so a user task executing `in` or `out`
  takes a #GP rather than talking to a device directly.

The one IDT gate ring 3 may enter is the syscall vector, which is the only
entry with DPL 3. A user task executing `int` on anything else takes a #GP
instead of reaching a handler never meant for it. Hardware-delivered interrupts
ignore DPL, so keeping every other gate at DPL 0 costs nothing.

`./corrode user-test` puts a ring 3 task in front of a kernel address and
requires the read to fault. It reports error code 0x5 — protection violation,
user mode — with `cs 0x23`, `ss 0x1b` and CR2 holding the kernel address it
reached for. Without that test the ring boundary is a claim about descriptor
bits rather than an observed property.

#### The syscall boundary takes no pointers

A syscall that takes a user pointer must validate it: in range, mapped,
user-accessible, and still all three when the kernel dereferences it. That
check is a classic place to get owned, and nothing here needs it. An IPC message
is a label and four words, so it crosses the boundary **by value, in registers**.
The kernel never follows a pointer the caller supplied, because the caller
cannot supply one.

That is a forcing function rather than a permanent restriction. Bulk transfer,
when it is needed, should be a shared Frame the receiver already holds a
capability for — authority it was granted, not an address it asserted.

The fourth argument goes in `r10` rather than `rcx`. Nothing requires that
today, since entry is through an `int` gate that preserves rcx, but `syscall`
clobbers rcx with the return address; using r10 now means fast syscalls later
will not change the ABI under existing user code.

Syscalls arrive through the ordinary trap path rather than `syscall`/`sysret`.
The stubs already normalise every entry into one `TrapFrame`, which is also what
a context switch saves and restores — so a syscall that blocks in IPC is just a
frame handed to somebody else, with no second entry path to keep in sync. Fast
syscalls are an optimisation to make against a mechanism that already works.

#### Where user code comes from

There is no loader and no filesystem, so a user program is a function in the
kernel image whose frames are mapped a second time at a user address, read and
execute, never writable. It follows that such a function must be entirely
self-contained: a call to another kernel function, or a `&str` in `.rodata`,
would be a reference to an address ring 3 cannot reach. The compiler has no way
to be told that, which is why user programs here are naked assembly — not for
speed, but because it is the only way to be certain the code touches nothing
but its own page and its own stack.

The demo task holds exactly one capability, a send-only endpoint. It is denied
when it calls through an empty slot, and the only way it can report that denial
is to say so through the capability it does hold. It then makes a real call and
gets an answer back. The check that it was genuinely in ring 3 reads the CS the
**CPU** wrote into the trap frame, which is the one claim the measured code
could not have faked.

Known gaps: there is still a single CR3. Kernel memory is unreachable from ring
3 because kernel mappings lack the USER bit, so the ring boundary holds — but
user tasks share the lower half with each other, so one could read another's
stack. Per-task address spaces are what fixes that, and they want a capability
for the address space itself rather than a flag on a task. There is also no
receive syscall yet: a ring 3 task can be a client but not a server, because a
server must also nominate slots for transferred and reply capabilities, and
that is a wider boundary than anything is tested against today.

### The scheduler

Preemptive round-robin over kernel threads. No userspace yet, so every task runs
in ring 0 and shares the address space; what differs is a stack and a saved
register set.

The context switch is one idea: **the interrupt frame is the saved context**.
`common_stub` already pushes every register on entry and pops them on exit, and
`iretq` reloads RSP as well as RIP. So a switch is just `trap_dispatch`
returning a pointer to a *different* frame — the epilogue that was going to
restore the interrupted task restores the next one instead. There is no separate
switch routine and no second copy of the register-save code to drift out of sync
with the first.

`yield_now()` raises a software interrupt rather than calling the scheduler
directly, so voluntary and involuntary switches go through exactly one path.

Locks shared with interrupt handlers are taken inside
`arch::x86_64::without_interrupts`. Taking one with interrupts enabled invites a
handler to fire mid-critical-section and spin forever on a lock its own
interruptee holds.

That includes the console. It is easy to read the serial lock as a formality —
one CPU, and a print is short — and it is the one that actually bites. A task
preempted mid-`println!` keeps the lock while it is descheduled; the next
context to want it with interrupts disabled then spins on a holder the
scheduler cannot resume, and the machine stops dead mid-line, nowhere near the
cause. `serial::_print` therefore runs entirely inside `without_interrupts`, so
nothing is ever descheduled owning the console. For the same reason `spawn`
logs the stack it just mapped *after* dropping the scheduler lock, rather than
being the one place in the kernel that takes the console lock underneath it.

### Stack guard pages

Every task stack is a mapped range with an **unmapped page directly below it**.
That is why stacks are not heap allocations: an overrun hits the guard and
faults at the instruction responsible, instead of quietly corrupting whatever
the allocator put underneath.

An overflow surfaces as a **double fault**, not a page fault — the stack is
already unusable, so the CPU cannot push the page-fault frame and the fault
faults. That report only exists because the #DF gate uses IST1 and gets a
known-good stack; without it this is a triple fault and an instant reset with
nothing printed. CR2 still holds the address of the original fault, so the
handler prints it.

Stacks are never reclaimed. Ranges are handed out by bumping a cursor, so a
system that spawned and exited many tasks would eventually exhaust the region.
Fixing it needs a free list plus a way to know no CPU is still running on the
stack; neither matters while tasks are created once at boot.

### Self-checks

Five run on every boot, so a regression shows up immediately rather than three
subsystems later: an `int3` that must trap *and resume*, a frame alloc/free
round-trip that must balance, a heap exercise that must both balance its bytes
and collapse back to a single free block, a timer check that counts both tick
sources against one second of polled wall time, and fifty-five capability, IPC
and ring 3 checks covering narrowing, denial, type confusion, the GRANT rule,
the rendezvous in both orderings, capability transfer, reply-capability expiry,
call refusal, and a user task being denied, answered, and confirmed to have
been in ring 3.

That last one earns its place. Deriving "elapsed seconds" from a tick counter
and then using it to judge that same counter is circular, and hides exactly the
case where a timer runs at the wrong rate — which is how a broken calibration
went unnoticed until it was measured against something independent.

Four more are opt-in because they end the boot: `./corrode fault-test` takes a
deliberate page fault to exercise trap reporting, `./corrode wx-test` is
described above, `./corrode stack-test` overflows a task stack to prove the
guard pages work, and `./corrode user-test` has a ring 3 task read a kernel
address to prove it cannot.
