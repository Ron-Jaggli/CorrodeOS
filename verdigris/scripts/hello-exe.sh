#!/usr/bin/env bash
# Write hello.exe: the smallest Windows program Verdigris's first phase
# runs, built byte by byte, with no Windows toolchain.
#
# A PE32+ console executable at 0x400000 with two sections and no imports:
# .text, which sends one line to the log endpoint in capability slot 0 with
# CorrodeOS's own system call (int 0x80) and exits; and .rdata, the line.
# A real Windows program never makes a CorrodeOS system call -- it calls
# kernel32, which is phase 2 -- but this one has nothing to call yet.
#
# The layout matches the builder in pe/src/tests.rs, whose last test runs
# this script and parses what it writes.
set -euo pipefail
out=${1:?usage: hello-exe.sh OUT}

bytes=""
# Little-endian fields, appended as \x escapes for one printf at the end.
u8() { bytes+=$(printf '\\x%02x' "$1"); }
u16() { u8 $(($1 & 0xff)); u8 $((($1 >> 8) & 0xff)); }
u32() { u16 $(($1 & 0xffff)); u16 $((($1 >> 16) & 0xffff)); }
u64() { u32 $(($1 & 0xffffffff)); u32 $((($1 >> 32) & 0xffffffff)); }
hex() { for b in "$@"; do u8 "0x$b"; done; }
zeros() { for ((i = 0; i < $1; i++)); do u8 0; done; }
# Zeros up to file offset $1.
pad_to() { local have=$(( ${#bytes} / 4 )); zeros $(($1 - have)); }

# DOS header: "MZ", and at 0x3c where the PE header is.
hex 4d 5a; zeros $((0x3c - 2)); u32 0x40

# PE signature and COFF header: x86-64, two sections, no symbols, a
# 240-byte optional header; executable, large-address aware, relocations
# stripped (it is only ever loaded at its base).
hex 50 45 00 00
u16 0x8664; u16 2; u32 0; u32 0; u32 0; u16 240; u16 0x0023

# PE32+ optional header.
u16 0x20b; u8 0; u8 0                 # magic, linker version
u32 0x200; u32 0x200; u32 0           # code, initialised, uninitialised data
u32 0x1000; u32 0x1000                # entry point, base of code
u64 0x400000                          # image base
u32 0x1000; u32 0x200                 # section and file alignment
u16 6; u16 0; u16 0; u16 0; u16 6; u16 0   # OS, image, subsystem versions
u32 0                                 # Win32VersionValue
u32 0x3000; u32 0x200; u32 0          # image size, headers size, checksum
u16 3; u16 0x0100                     # console subsystem; NX compatible
u64 0x100000; u64 0x1000              # stack reserve, commit
u64 0x100000; u64 0x1000              # heap reserve, commit
u32 0; u32 16                         # loader flags, sixteen directories
zeros $((16 * 8))                     # every directory empty: no imports

# Section table.
hex 2e 74 65 78 74 00 00 00           # .text
u32 62; u32 0x1000; u32 0x200; u32 0x200; u32 0; u32 0; u16 0; u16 0
u32 0x60000020                        # code, execute, read
hex 2e 72 64 61 74 61 00 00           # .rdata
u32 32; u32 0x2000; u32 0x200; u32 0x400; u32 0; u32 0; u16 0; u16 0
u32 0x40000040                        # initialised data, read
pad_to 0x200

# .text, at RVA 0x1000. Send the 32 bytes at .rdata as one log line
# (SYS_SEND, label log::LINE, words in rdx r10 r8 r9), then SYS_EXIT.
hex 48 8d 05 f9 0f 00 00              # lea rax, [rip + 0xff9]   ; .rdata
hex 48 8b 10                          # mov rdx, [rax]
hex 4c 8b 50 08                       # mov r10, [rax + 8]
hex 4c 8b 40 10                       # mov r8, [rax + 16]
hex 4c 8b 48 18                       # mov r9, [rax + 24]
hex 31 ff                             # xor edi, edi             ; slot 0
hex be 02 00 00 00                    # mov esi, 2               ; log::LINE
hex 49 c7 c4 ff ff ff ff              # mov r12, -1              ; no capability
hex 45 31 ed                          # xor r13d, r13d
hex 49 c7 c6 ff ff ff ff              # mov r14, -1              ; none back
hex b8 02 00 00 00                    # mov eax, 2               ; SYS_SEND
hex cd 80                             # int 0x80
hex b8 0c 00 00 00                    # mov eax, 12              ; SYS_EXIT
hex cd 80                             # int 0x80
hex 0f 0b                             # ud2                      ; never reached
pad_to 0x400

# .rdata, at RVA 0x2000: the line, NUL-padded to 32 bytes.
line="hello from a PE, over int 0x80"
for ((i = 0; i < ${#line}; i++)); do u8 "'${line:i:1}"; done
zeros $((32 - ${#line}))
pad_to 0x600

printf "$bytes" > "$out"
