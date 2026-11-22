# Verdigris

Windows x86-64 programs on CorrodeOS. Not Wine and not Proton: a
compatibility layer written for this kernel, on its capabilities and IPC.
It has no Linux system calls, no POSIX layer and no prefix. A Windows program
is one CorrodeOS task. It holds only what a native program in its place would
hold, and it sees the real files through a pruned view: its own folder, plus
whatever it was allowed. Everything else is absent, not denied.

[DESIGN.md](DESIGN.md) is the design, with the decisions taken and the ones
still open. CorrodeOS's PROGRESS.md (sections 73 to 97 onward) is the log of
how it was built.

## What runs today

Console programs, including C programs compiled the ordinary way by
mingw-w64's gcc (`verdigris/c/hello.c`, a test program). A program
loads, is moved if it was linked elsewhere, has its imports bound by the
kernel, and starts in kernel32 as on Windows: its TLS callbacks run, then
its entry, and what that returns is its exit code. It runs in ring 3 seeing
its own folder as `C:\`, reads its TEB and PEB through `gs` as Windows code
does, is stopped if it runs past 10 s, and is told how it ended.

- **kernel32**: start-up, files through the view (`CreateFileW`,
  `ReadFile`, `FindFirstFileW` and the rest), standard output, the command
  line, the process heap, thread-local slots, critical sections and
  semaphores (for one thread), UTF-8 code-page conversion, `Sleep` and the
  performance counter. 42 functions.
- **msvcrt**, in Rust: what a mingw-built program's start-up and a console
  program use: `__getmainargs` (Microsoft's argument rules), `_initterm`,
  `atexit`/`exit`, the standard streams, the `printf` family (its own
  formatter), `malloc`/`free`, strings, the C locale. 53 functions and
  variables.
- **Not yet**: threads, exceptions (a fault ends the program), other DLLs
  and `LoadLibrary`, writing files, `stdin`, wildcard arguments, windows.

From the CorrodeOS terminal: `win /programs/c/hello.exe one "two three"`,
or `win /programs/dir/dir.exe`. In Files, select a `.exe` and press
Return.

## Layout

```
verdigris/
  DESIGN.md        the design
  pe/              corrode-pe: reads a PE file, checks it whole, gives its
                   segments, fixups (relocations, bound imports) and entry;
                   host-tested against malformed and fuzzed files
  view/            corrode-view: the pruned view, Windows paths, searches;
                   host-tested
  fileview/        the ring 3 service holding one program's view
  kernel32/        Verdigris's kernel32, in Rust (a CorrodeOS image at a
                   fixed address carrying an export table, not a PE DLL yet)
  dir/             dir.exe, a test program built by rustc and rust-lld
  c/               hello.c, a test program built by mingw-w64's gcc
  scripts/         the other test programs: hello.exe and console.exe written
                   byte by byte, reader/spin/fault.exe assembled by GNU as,
                   and dir-exe.sh
```

Outside this folder, in CorrodeOS, and staying there:

- `kernel/src/verdigris.rs`: the boot checks for phases 1 to 3, starting a
  program with its view (`start_windows`), `window::RUN` (`run_windows`),
  and the watcher that reports how a program ended.
- `kernel/src/task/user.rs`: loading a PE (`load_pe`, `load_windows`: the
  TEB, PEB and TLS block too). It shares its image building with the ELF
  loader, so it lives beside it. A task's GS base is kept per task in
  `kernel/src/task/mod.rs`.
- `abi/src/lib.rs`: `verdigris` (slots and addresses) and `view` (the
  view's calls).
- `user/rt/src/win.rs`: the run protocol the terminal and Files use.

## Can it be a project of its own?

As a folder of its own, yes. As a project independent of CorrodeOS, no.
Every part below the cut speaks CorrodeOS's IPC and holds its capabilities,
and the loader runs in its kernel. What it needs from CorrodeOS is small and
named above: `abi`, `user/rt`, `elf` (for `pe`), `shell` (for `view`'s
pattern matching) and the kernel's half.

## Building and testing

Everything goes through CorrodeOS's `./corrode`:

```sh
./corrode test       # host tests, corrode-pe and corrode-view among them
./corrode pe-test    # boot: phases 1-3, the kernel's checks and reader.exe
./corrode win-test   # boot: `win` in the terminal, nine runs, four programs
./corrode files-run-test
```

Add `GPU=virtio` for the other GPU. Both must pass on both.

## Licence

MPL-2.0, as CorrodeOS. No Wine code is in any file here. DESIGN.md
section 5 says how Wine's PE DLLs may be used later, as separate LGPL files.
