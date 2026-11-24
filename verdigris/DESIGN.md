# Verdigris: Windows programs on CorrodeOS

Verdigris runs Windows x86-64 programs on CorrodeOS. It is a Windows
compatibility layer written for this kernel, not a port of one written for
another. This document is the design; there is no code yet.

## Why not Proton, or Wine as it is

Proton is Wine plus DXVK plus Steam glue, and all three stand on Linux:

- **Wine's lower half is a Linux program.** Since Wine 7 every DLL is split
  in two: a PE half that runs as Windows code, and a "Unix half" (an ELF
  shared object, `ntdll.so`, `win32u.so`, `winex11.drv.so`...) that it
  reaches through `__wine_unix_call`. The Unix halves call glibc,
  pthreads, `mmap`, signals, X11/Wayland, FreeType and OpenGL/Vulkan.
  CorrodeOS has no Linux system calls, no POSIX layer, no dynamic linker
  and no Vulkan. None of the Unix halves can run here, and porting them
  means writing a POSIX layer first -- a second operating system's worth
  of ambient authority this kernel exists not to have.
- **DXVK is Direct3D on Vulkan.** There is no Vulkan (see Direct3D below).
- **A Wine prefix is a hidden filesystem.** `drive_c`, the registry files
  and `dosdevices` live in a directory the program treats as a whole disk.
  Verdigris has no prefix (see Files below).

So Verdigris keeps exactly one idea from Wine's design, the split between
Windows code above and the system below a narrow call layer, and puts
CorrodeOS under that layer instead of Linux.

## Shape

```
  program.exe  +  its DLLs                   (Windows code, unmodified)
  ─────────────────────────────────────────
  Wine's PE DLLs, unmodified (LGPL):          user32, gdi32 (PE half),
    msvcrt, ucrtbase, shlwapi, comctl32,      advapi32, ...
    ...anything that only calls other DLLs
  ───────────────── the cut ─────────────────  (Nt*, NtUser*, NtGdi* calls)
  Verdigris's own DLLs, in Rust (MPL-2.0):
    ntdll (the Nt* calls), kernel32/kernelbase subset early on,
    win32u (NtUser*/NtGdi*: windows, input, drawing)
  ─────────────────────────────────────────
  CorrodeOS: capabilities and IPC             (int 0x80, never `syscall`)
    window server, file view, log, pool, clock
```

Everything above the cut runs as it would on Windows. Everything below it
is ordinary CorrodeOS ring 3 code holding ordinary capabilities. A
Windows program never makes a Corrode system call itself; Verdigris's DLLs
do, in the program's own task, with the program's own capabilities. A
program that tries the `syscall` instruction directly (as Windows' own
ntdll does) gets an invalid-opcode fault: CorrodeOS never enables
`syscall`/`sysret`, so there is no second way in.

One Windows process is one CorrodeOS task. It can do exactly what its
capabilities allow, and Verdigris adds nothing a native program in the
same seat could not do. That is the security property everything below is
held to: **no Verdigris feature may need a system call that widens what
ring 3 can do** without that being decided on its own, in the open. Where
one would, this document says so and leaves it as a decision.

Nothing here touches anticheat, integrity checks, or kernel drivers of any
kind. Programs that need them are not a goal and will not be worked on.

## 1. Loading a PE: on the `spawn_program` model

Today `task::spawn_program(name, file)` takes an ELF file, has
`corrode_elf::Elf::parse` check it entirely before a frame is taken, maps
its segments into a fresh address space, and starts it in ring 3 holding
no capabilities; whoever spawned it grants what it needs. A PE is loaded
the same way.

**`corrode-pe`**, a `no_std` crate like `corrode-elf`, host-tested,
including on malformed and fuzzed input. `Pe::parse(bytes, allowed)`
accepts only:

- a DOS header whose `e_lfanew` points inside the file at `PE\0\0`;
- machine `0x8664` (x86-64) and a PE32+ optional header -- no 32-bit
  programs (see Not in scope);
- `SectionAlignment` of 4096 (a page) and a `FileAlignment` that divides
  it; `SizeOfImage` and `SizeOfHeaders` consistent with the sections;
- sections that lie inside the file (raw data) and inside the image
  (virtual range), in order, not overlapping, each with its bytes
  zero-filled past `SizeOfRawData`;
- **no section both writable and executable** -- the rule `corrode-elf`
  already enforces, and the reason no Windows program can make code at
  run time (see Memory);
- an entry point inside an executable section;
- an image that fits in `layout::IMAGE_BASE..IMAGE_END` at its preferred
  base, or that has base relocations (`IMAGE_REL_BASED_DIR64` only;
  anything else refuses the file) to move it there.

What comes out is the same thing `Elf::parse` gives: a list of segments
(where, how long, which bytes, which permissions) and an entry point, plus
a list of 64-bit fixups (relocations, and later bound imports) to write
into the segments' frames **before** they are mapped with their final
permissions. The kernel never maps a page writable for ring 3 in order to
patch it afterwards.

`task::spawn_pe(name, file)` is `spawn_program` with `Pe::parse` in place
of `Elf::parse`. Like a driver today, the first PEs come from the boot
modules, not from the person's files (phase 1).

Win64 entry differs from the System V entry the ELF path sets up: the
stack must have 32 bytes of shadow space above the return address and
`rsp % 16 == 8` at the first instruction. The return address points at a
stub that exits the task, so a `main` that returns ends the process as
Windows' does.

**Decision before running PEs from the person's files (not phases 1-2):**
parsing an untrusted file in the kernel is attack surface. `corrode-elf`
does it only for boot modules. Either keep parsing in the kernel
(`corrode-pe` is small, bounds-checked and fuzzed, like `corrode-elf`), or
move it to a ring 3 loader service that hands the kernel a pre-checked
plan, which the kernel re-checks (ranges, W^X, overlap) and maps. The
second needs a capability-gated "spawn from plan" call held only by that
service. It is a new system call with security weight, so it is not
chosen here.

## 2. ntdll and kernel32: the smallest useful subset

Verdigris's ntdll, kernel32 and kernelbase are its own, written in Rust,
in the program's task. They keep the process's state in ring 3: a handle
table (handle to object: a capability slot, or a Verdigris object such as
an event), the process heap, the environment and the command line.

**How the DLLs are built.** A Verdigris DLL is a real PE DLL with a real
export table, so `GetProcAddress`, `GetModuleHandle` and programs that walk
the module list all work. Rust's `x86_64-unknown-uefi` target emits PE32+
images with `rust-lld`, which the build image already ships -- no Windows
toolchain, no mingw. It is not installed in the pinned toolchain today;
adding it is a container image change (`rustup target add` when the image
is built), recorded as a step of its own when it is needed. Until then
(phase 2) the smallest kernel32 can be a fixed-address image whose export
table the kernel's PE loader reads, or hand-assembled bytes, whichever is
smaller to check.

**Binding imports.** The kernel's PE loader resolves a program's import
table against the exports of Verdigris's DLLs, which it maps into the same
address space, and writes the import address table into the frames before
mapping them (the fixups above). Unresolved imports refuse the program at
load, by name, in the log; there is no stub that pretends.

The program's own DLLs, the ones in its folder that its imports name, are
loaded the same way at start: the whole set is checked and mapped before
the first instruction runs. `LoadLibrary` after start is different: it
asks for code to be mapped on demand, and a program that can write a file
in its own folder and then map it as code has, in effect, a JIT. Until
that is decided (it is the last decision at the end), `LoadLibrary`
succeeds only for DLLs already loaded at start, and returns
`ERROR_MOD_NOT_FOUND` for the rest. Many programs list their plug-ins in
their imports or probe for optional DLLs and carry on without them; ones
that require run-time loading will not run yet.

**Phase 2's subset** is three calls, and needs no capability a native
program lacks:

| Call | On CorrodeOS |
| --- | --- |
| `GetStdHandle` | pseudo-handles for stdin, stdout, stderr |
| `WriteFile` | to stdout or stderr: the log endpoint (`app::LOG`), 32-byte pieces as `corrode_rt::log` sends them |
| `ExitProcess` | `SYS_EXIT` |

After that, in the order programs need them:

- **Memory.** `VirtualAlloc`/`HeapAlloc` take pages from the program's
  pool (`SYS_ALLOC`) and map them (`SYS_MAP`). The kernel chooses
  addresses, so a request for a fixed address succeeds only if it happens
  to match; the rest get `ERROR_INVALID_ADDRESS`. `PAGE_EXECUTE*` is
  refused: CorrodeOS has no call that makes ring 3 memory executable, and
  Verdigris does not add one. This rules out JIT compilers (.NET, Java,
  LuaJIT, browsers' JavaScript). Allowing them would mean a new system call
  that lets ring 3 create code, which is a security decision for the person
  who owns CorrodeOS, not a compatibility detail.
- **Time.** `QueryPerformanceCounter` is `SYS_CLOCK` (nanoseconds; the
  frequency is reported as 10^9). `Sleep` is `SYS_SLEEP`.
  `GetSystemTimeAsFileTime` needs the wall clock, which today only the
  panel hears (`window::CLOCK`). The launcher can pass it at start as the
  pair the panel gets (seconds since 1970 at a monotonic nanosecond), from
  which Verdigris computes the time from then on. That is an IPC message at
  start, not a new system call.
- **Files.** Through the file view capability (section 4): `CreateFileW`,
  `ReadFile`, `WriteFile`, `FindFirstFileW` and the rest, as calls on it.
- **Threads.** CorrodeOS has one task per address space; there are no ring
  3 threads. `CreateThread` needs the kernel to start a second task in an
  existing address space, with its own stack and register state. That is a
  new kernel feature and a new system call. It is needed by nearly every
  real program, and it is decided when phase 3 starts, not here.
- **The TEB and PEB.** Windows code reads `gs:[0x30]` (the TEB) and
  `gs:[0x60]` (the PEB) without asking; MSVC's `__chkstk` and stack cookie
  code do it in every program. The kernel would have to keep a per-task
  user GS base and restore it on every switch. That is a kernel change, and
  is also decided with threads.
- **Exceptions.** Access violations and the like are delivered to ring 3
  on Windows (`KiUserExceptionDispatcher`), where SEH unwinding (in the PE
  ntdll half, using the image's `.pdata`) finds a handler. On CorrodeOS a
  ring 3 fault ends the task. Delivering faults to the task that caused
  them is a kernel change with security weight, decided when it is needed.

## 3. user32 and gdi32: onto the window server and command lists

Wine's user32 and gdi32 PE halves are thin: almost everything they do is
an `NtUser*` or `NtGdi*` call into win32u. Verdigris's win32u implements
those calls on CorrodeOS.

**Windows.** Each top-level `HWND` is one window opened with
`window::OPEN` on the program's window endpoint (`app::WINDOWS`), with
`RESIZABLE` when its style says so. Child windows (controls, panes) are
not windows on the desktop. Verdigris composes them into their top-level
window's surface itself, as Windows did before DWM. The window server
then sees one surface per top-level window. Its limit of 32 surfaces for
everything (section 69) is what bounds a program's top-level windows.

**Messages.** `GetMessage`/`PeekMessage` are `SYS_WAIT` on the window's
capability, with the deadline from the timer queue (`SetTimer`). The
window server's events translate directly:

| Window server | Windows |
| --- | --- |
| `KEY` (set-1 code, pressed, char, modifiers) | `WM_KEYDOWN`/`WM_KEYUP`, `WM_CHAR`, `WM_SYSKEYDOWN` with Alt; virtual-key codes from the set-1 code |
| `BUTTON` | `WM_LBUTTONDOWN`/`UP`, `RBUTTON`, `MBUTTON`, and `WM_*DBLCLK` by timing |
| `SCROLL` | `WM_MOUSEWHEEL`, 120 a notch, sign flipped |
| `FOCUS` | `WM_SETFOCUS`/`WM_KILLFOCUS`, `WM_ACTIVATE` |
| `RESIZE` | `WM_SIZE`, then `WM_PAINT` for all of it |
| `CLOSE` | `WM_CLOSE` (whose default, `DestroyWindow`, closes it) |
| `THEME` | `WM_SETTINGCHANGE` / `WM_SYSCOLORCHANGE` |

Missing today, and needed: the window server tells a window nothing about
the pointer moving over it, only `HOVER` to popovers, so there is no
`WM_MOUSEMOVE`. That is a new window-server event (an IPC label, like
`LABEL_POINTER_AT` for the xHCI driver), not a system call. Titles are
eight ASCII bytes plus a 24-byte subtitle. A Windows title goes in the
subtitle, and the title the desktop shows comes from the launcher, as
section 46 already wants for every program.

**Drawing.** A device context records `corrode-render` commands into a
`CommandList` and the software renderer, which is `no_std` and runs in
ring 3, executes them on the window's mapped surface; then `SYS_COMMIT`
says which rectangle changed. GDI maps onto the commands there are:

| GDI | corrode-render |
| --- | --- |
| `FillRect`, solid brushes, `PatBlt` | `FillRect` |
| alpha (`AlphaBlend` with a constant) | `BlendRect` / `Image` with alpha |
| `BitBlt`, `SetDIBitsToDevice`, DIB sections | `Image` from an in-process pixel buffer |
| `StretchBlt`, `StretchDIBits` | `ScaledImage` |
| clipping regions (rectangles) | `Scissor` |
| `TextOut`, `DrawText` | glyphs from `corrode-text` as `Mask`/`ScaledMask` |

Lines, ellipses, polygons, paths and raster operations other than
`SRCCOPY` have no command. They are rasterised by Verdigris into a pixel
buffer (the `gfx` crate's rasteriser can do this) and drawn as an `Image`.
Fonts are whatever `corrode-text` has. A program asking for "Arial" gets
Corrode's face, with the metrics GDI reports computed from it.

No Direct2D, DirectWrite or DWM composition. Those are above GDI or need
a GPU (next section).

## 4. Files: a pruned view, never a hidden disk

There is no prefix, no `drive_c` and no file system of Verdigris's own. A
Windows program sees the real CorrodeOS files, the same ones the terminal
and Files show, but only those it has been allowed.

**What it is allowed.** Its own folder (the folder its `.exe` is in),
plus a list of directories and files, each read-only or read-write. The
list is the person's: it is given at launch, by the launcher, and changed
only by the person. Opening a file through the system's file dialog adds
that file to the running program's view; this is the "powerbox" pattern,
and the dialog is the desktop's, not the program's. A program never
widens its own view.

**What it sees.** A view is the tree of the real files, pruned:

- An **allowed directory** shows everything in it, as it is.
- An **allowed file** shows only itself.
- A directory **on the way** to something allowed shows only the entries
  that lead to something allowed.
- **Anything else is not there.** It is absent from listings, and opening
  it fails with `ERROR_FILE_NOT_FOUND` or `ERROR_PATH_NOT_FOUND`, exactly
  as for a name that does not exist. It is never
  `ERROR_ACCESS_DENIED`: a program cannot tell a hidden file from a
  missing one, so the view does not even say what exists beside it.

**Where its root is.** The view's root zooms out only as far as it must:
it is the deepest directory that contains everything allowed (the
program's own folder included). Examples, for a program in `/apps/robot`:

| Allowed (besides `/apps/robot`) | View root | `C:\` lists |
| --- | --- | --- |
| nothing | `/apps/robot` | the program's own files |
| `/apps/robot/save.dat` | `/apps/robot` | the same |
| `/apps/shared/fonts` | `/apps` | `robot`, `shared` (and in `shared`, only `fonts`) |
| `/root/dih/robot.txt`, `/user/dih/robo.font` | `/` | `apps`, `root`, `user`; in `root` only `dih`, in it only `robot.txt`; in `user` only `dih`, in it only `robo.font` |

**Windows paths.** `C:\` is the view's root. There is one drive. Paths
are made absolute against the current directory (initially the program's
folder), and `.` and `..` are resolved in the view: `..` at the root stays
at the root, as `C:\..` does on Windows. `\\?\C:\...` is the same path;
other drives, UNC paths (`\\server\...`) and device names (`\\.\...`,
`CON`, `NUL`) are not files (`NUL` and `CON` are handled by kernel32
itself). `/` is accepted as `\`. Matching is case-insensitive, as on
Windows. Where two real names differ only in case, both are listed, and an
open picks an exact match first, then the first in sorted order.
`GetFullPathName` and friends return `C:\` paths, never Corrode ones, so
a program never learns where its view sits in the real tree.

**Writing.** Inside an allowed read-write directory or file, writes go to
the real file. Everywhere else in the view nothing can be created or
changed. A directory shown only on the way to something allowed behaves
as read-only media: creating there fails with `ERROR_WRITE_PROTECT`, not
with access denied. Anything wholly outside the view does not exist, so
creating there fails with `ERROR_PATH_NOT_FOUND`.

**Where Windows expects its own places:**

| Windows expects | On CorrodeOS |
| --- | --- |
| `%APPDATA%`, `%LOCALAPPDATA%`, the Saved Games folder | a folder in the program's own folder, `AppData\`, real and visible |
| `%TEMP%` | `AppData\Temp\`, emptied when the program exits |
| `C:\Windows\System32` | not in the view; DLLs load by name through the loader, not through files the program can see |
| the registry | in memory, seeded from a read-only hive shipped with Verdigris; a program's changes are kept, if at all, as a visible file in its own folder |

**As a capability.** A view is an endpoint, held by the program
send-only, and served by a ring 3 file service holding the real tree.
Today that tree is the read-only archive packed from `root/`, and later
whatever store holds the person's files. The calls are `OPEN` (a path,
relative to the view root, in a page of the program's memory), `READ`,
`WRITE`, `LIST`, `STAT` and `CLOSE`, with bulk data in a memory
capability granted once, as sealing already does (section 66). The
service resolves every path against the view's allowed set itself. The
view holds no path the program could reach around, and a symbolic link,
when the store has them, is followed in the real tree and then checked
against the allowed set like any other path.

Decided when phase 3 built it (PROGRESS section 82):

- **one task per view.** Endpoints carry no badge, so the service runs
  once for each program it serves: `fileview`, holding that view's
  endpoint, the tree, the list and the page, and nothing else. It ends
  when the program does (kernel32's `ExitProcess` tells it). No kernel
  feature was needed.
- **the list is the service's, in a read-only page.** The program never
  holds it, so it cannot widen it, and the service copies each path out
  of the shared page before reading it.
- where the person's allowlists are kept between boots is still open; it
  comes with the launcher starting Windows programs from Files. In the
  store, in the clear: nothing on disk is encrypted with the per-boot
  key.

## 5. Reusing Wine's PE DLLs, and the licences

**What is reused.** Wine's DLLs above the cut, as PE files and
unmodified: the C runtimes (`msvcrt`, `ucrtbase`), `shlwapi`, `shell32`'s
pure parts, `comctl32`, `comdlg32`'s PE half, `advapi32` (registry calls
go down to Verdigris's ntdll), `ole32`/`oleaut32`, and user32's and
gdi32's PE halves. Wine's ntdll, kernel32/kernelbase and win32u are
replaced by Verdigris's own, because those are the DLLs whose Unix halves
are Linux. Wine's display drivers (`winex11.drv`, `winewayland.drv`),
`wined3d` and `winevulkan` are not used at all.

Building Wine's PE DLLs needs a compiler that targets Windows (mingw-w64
or llvm-mingw). That is a build-image change, made in a separate build
step whose output is the DLL files, so the CorrodeOS build itself stays
as it is.

**Licences.** CorrodeOS is MPL-2.0; Wine is LGPL-2.1-or-later.

- **Wine's DLLs stay LGPL, as separate files.** Verdigris loads them at
  run time, which is the dynamic linking the LGPL permits. Whoever
  distributes them must give their source (Wine's, at the version used)
  or a written offer of it, keep the LGPL notices, and let the person
  replace them with their own builds, which separate DLL files already do.
- **Changes to a Wine DLL are LGPL and are published.** The aim is to need
  none: Verdigris sits under the DLLs' existing call layer, so the
  DLLs need no Verdigris-specific code.
- **Verdigris's own DLLs are MPL-2.0 and contain no Wine code.** They
  implement the same calls the DLLs above make, an interface, written from
  Microsoft's documentation and from the behaviour of the DLLs above; an
  independent implementation of an interface is not a work based on the
  library. If code from Wine is ever needed in one, it goes in a file of
  its own under the LGPL, never pasted into an MPL file.
- **The two licences mix.** MPL-2.0 is file-level copyleft and, without
  an "Incompatible With Secondary Licenses" notice (CorrodeOS has none),
  allows MPL files in a larger work with (L)GPL ones (MPL-2.0 sections 3.3
  and 10). A distribution of CorrodeOS with Verdigris and Wine's DLLs is
  such a work: each file keeps its own licence.
- **Microsoft's DLLs are never shipped**, and Verdigris does not load
  them in place of its own.

This is an engineer's reading, not legal advice. Before anything is
distributed with Wine's DLLs in it, someone who answers for the project
should confirm it.

## 6. Direct3D: what a GPU backend must give (out of scope)

Verdigris does not do Direct3D now. CorrodeOS has no GPU 3D path: the
virtio-gpu and NVIDIA drivers present 2D surfaces, and `corrode-render`'s
commands are fixed 2D operations. Wine's `wined3d` needs OpenGL or Vulkan
and DXVK needs Vulkan, and Verdigris uses neither. For Direct3D 11/12, a
future CorrodeOS GPU backend has to provide, natively:

- **Per-process GPU memory**: buffers and textures allocated from a
  capability, mapped into the process for upload, with GPU virtual
  addresses only that process's work can use.
- **A shader path**: DXBC/DXIL translated (in ring 3, by Verdigris) into
  the backend's own IR, and that IR compiled to the GPU's machine code by
  the driver. Compiled shaders are data the GPU runs, not code ring 3
  runs, so the no-executable-memory rule holds.
- **Pipeline state objects, descriptor heaps and root signatures**: these
  map naturally onto an explicit API, so D3D12 is the easier target and
  D3D11 sits on top of it, as `vkd3d`-style layers do.
- **Command lists as plain data**, submitted with explicit fences:
  `corrode-render`'s command list principle, extended to draws,
  dispatches, copies and barriers.
- **Presentation**: a swap chain as a set of window surfaces the window
  server flips between, with frame pacing from the compositor's clock.
- **A software reference backend** first, as `corrode-render` has one:
  the `gfx` crate's software rasteriser grown to run translated shaders,
  so correctness never waits on hardware.

## Not in scope

- 32-bit programs. They need a 32-bit code segment and thunks (Wine's
  `wow64` design), a kernel change of its own.
- Kernel drivers, services, anything that installs itself.
- Anticheat and integrity checks of any kind. No work, no workarounds.
- `.NET`, Java and other JIT runtimes, unless the no-executable-memory
  decision in section 2 is changed.
- Networking (CorrodeOS has none), sound, printers.

## Phases

1. **A PE loads.** `corrode-pe` with host tests, malformed and fuzzed
   files refused, never crashed on; `spawn_pe` from a boot module; a tiny
   PE whose bytes are built by a script (no Windows toolchain) prints over
   a CorrodeOS system call; a boot test on both GPUs. The test PE makes
   the Corrode system call itself, which real PEs never do.
2. **Hello through kernel32.** Imports bound at load against Verdigris's
   kernel32 subset (`GetStdHandle`, `WriteFile`, `ExitProcess`), stdout on
   the log endpoint. No capability beyond what every program has.
3. **Threads, TEB/PEB and exceptions.** Each needs a kernel decision
   (section 2), taken before any code.
4. **Files.** The view service and its capability (section 4).
5. **Windows and GDI.** win32u on the window server and `corrode-render`,
   with Wine's user32/gdi32 PE halves above it; a pointer-motion event
   from the window server.
6. **Wine's DLLs.** The separate build step, and the licence check before
   anything containing them is distributed.

## Decisions taken here

- **Choice:** the cut is at the Nt*/NtUser*/NtGdi* call layer. Below it
  is Verdigris's own Rust; above it is Wine's unmodified PE DLLs. Wine's
  Unix halves are never ported.
- **Choice:** one Windows process is one CorrodeOS task, holding only
  capabilities a native program in its place would hold. Verdigris
  never adds authority.
- **Choice:** PEs are loaded like ELF programs: checked whole before a
  frame is taken, W^X, inside the image range, fixups written before
  mapping. No ring 3 code creation, so `PAGE_EXECUTE*` allocations are
  refused.
- **Choice:** no prefix and no hidden file system: a pruned view of the
  real files, rooted at the deepest common directory, `C:\` at its root,
  where whatever is not allowed is absent (not found), never denied.
  Creating in a directory shown only on the way to something allowed
  fails as write-protected media.
- **Choice:** per-program Windows places (`AppData`, `Temp`, the
  registry's changes) are real, visible folders and files in the
  program's own folder.

## Decisions left to the person who owns CorrodeOS

Each of these has security weight or widens what ring 3 can do. None is
taken here, and none is needed for phases 1 and 2:

1. Parse PEs from the person's files in the kernel, or in a ring 3
   loader service with a spawn-from-plan capability (section 1).
2. Ring 3 threads: a task sharing another's address space (section 2).
3. A per-task user GS base, for the TEB (section 2).
4. Delivering a ring 3 fault to the task that caused it (section 2).
5. Whether ring 3 may ever create executable memory (JIT) (section 2).
6. Mapping DLL images at run time (`LoadLibrary` after start), and from
   which capabilities, since a program that can write a file it can then
   map as code has, in effect, a JIT (section 2 and 5).
