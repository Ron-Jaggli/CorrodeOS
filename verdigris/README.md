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

Small console programs. A program loads, is moved if it was linked
elsewhere, has its imports bound by the kernel, runs in ring 3 seeing its
own folder as `C:\`, is stopped if it runs past 10 s, and is told how it
ended (its exit code, or its fault named as Windows names it).

- **kernel32**, 16 functions: `GetStdHandle`, `WriteFile`, `ExitProcess`,
  `GetCommandLineW`, `CreateFileW`, `ReadFile`, `GetFileSize`,
  `CloseHandle`, `GetLastError`, `SetLastError`, `FindFirstFileW`,
  `FindNextFileW`, `FindClose`, `GetCurrentDirectoryW`,
  `SetCurrentDirectoryW`, `GetFullPathNameW`.
- **Not yet**: a C runtime, other DLLs, threads, TLS, the TEB and PEB,
  exceptions, memory allocation, writing files, input, windows.

From the CorrodeOS terminal: `win /programs/dir/dir.exe` (or `win PATH
ARGS`). In Files, select a `.exe` and press Return.

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
  scripts/         the other test programs: hello.exe and console.exe written
                   byte by byte, reader/spin/fault.exe assembled by GNU as,
                   and dir-exe.sh
```

Outside this folder, in CorrodeOS, and staying there:

- `kernel/src/verdigris.rs`: the boot checks for phases 1 to 3, starting a
  program with its view (`start_windows`), `window::RUN` (`run_windows`),
  and the watcher that reports how a program ended.
- `kernel/src/task/user.rs`: loading a PE (`load_pe`). It shares its image
  building with the ELF loader, so it lives beside it.
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
