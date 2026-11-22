#!/usr/bin/env bash
# Write console.exe: the Windows "hello" of Verdigris's second phase, built
# byte by byte, with no Windows toolchain.
#
# Unlike hello.exe it makes no CorrodeOS system call of its own. It does
# what a Windows console program does: imports GetStdHandle, WriteFile and
# ExitProcess from KERNEL32.dll and calls them through its import address
# table, which the kernel fills from Verdigris's kernel32 before the
# program's pages are mapped.
#
# The layout matches `importing` in pe/src/tests.rs, whose test runs this
# script and parses and binds what it writes.
set -euo pipefail
out=${1:?usage: console-exe.sh OUT}

bytes=""
u8() { bytes+=$(printf '\\x%02x' "$1"); }
u16() { u8 $(($1 & 0xff)); u8 $((($1 >> 8) & 0xff)); }
u32() { u16 $(($1 & 0xffff)); u16 $((($1 >> 16) & 0xffff)); }
u64() { u32 $(($1 & 0xffffffff)); u32 $((($1 >> 32) & 0xffffffff)); }
hex() { for b in "$@"; do u8 "0x$b"; done; }
zeros() { for ((i = 0; i < $1; i++)); do u8 0; done; }
pad_to() { local have=$(( ${#bytes} / 4 )); zeros $(($1 - have)); }
text() { local s=$1; for ((i = 0; i < ${#s}; i++)); do u8 "'${s:i:1}"; done; }

# DOS header, PE signature, COFF header: as hello.exe.
hex 4d 5a; zeros $((0x3c - 2)); u32 0x40
hex 50 45 00 00
u16 0x8664; u16 2; u32 0; u32 0; u32 0; u16 240; u16 0x0023

# PE32+ optional header.
u16 0x20b; u8 0; u8 0
u32 0x200; u32 0x200; u32 0
u32 0x1000; u32 0x1000
u64 0x400000
u32 0x1000; u32 0x200
u16 6; u16 0; u16 0; u16 0; u16 6; u16 0
u32 0
u32 0x3000; u32 0x200; u32 0
u16 3; u16 0x0100
u64 0x100000; u64 0x1000
u64 0x100000; u64 0x1000
u32 0; u32 16
# Directories: 1 is the import table, 12 the import address table.
zeros 8                               # 0 exports
u32 0x2000; u32 40                    # 1 imports: one descriptor and a zero one
zeros $((10 * 8))                     # 2-11
u32 0x2060; u32 32                    # 12 the import address table
zeros $((3 * 8))                      # 13-15

# Section table.
hex 2e 74 65 78 74 00 00 00           # .text
u32 61; u32 0x1000; u32 0x200; u32 0x200; u32 0; u32 0; u16 0; u16 0
u32 0x60000020                        # code, execute, read
hex 2e 72 64 61 74 61 00 00           # .rdata
u32 0x124; u32 0x2000; u32 0x200; u32 0x400; u32 0; u32 0; u16 0; u16 0
u32 0x40000040                        # initialised data, read
pad_to 0x200

# .text, at RVA 0x1000. On entry rsp is 8 below a 16-byte boundary; taking
# 0x38 leaves it aligned for the calls, with 32 bytes of shadow space, the
# fifth argument at rsp+0x20 and a place for "bytes written" at rsp+0x30.
hex 48 83 ec 38                       # sub rsp, 0x38
hex b9 f5 ff ff ff                    # mov ecx, -11             ; STD_OUTPUT_HANDLE
hex ff 15 51 10 00 00                 # call [rip + 0x1051]      ; GetStdHandle (0x2060)
hex 48 89 c1                          # mov rcx, rax             ; the handle
hex 48 8d 15 e7 10 00 00              # lea rdx, [rip + 0x10e7]  ; the text (0x2100)
hex 41 b8 24 00 00 00                 # mov r8d, 36              ; its length
hex 4c 8d 4c 24 30                    # lea r9, [rsp + 0x30]     ; bytes written
hex 48 c7 44 24 20 00 00 00 00        # mov qword [rsp + 0x20], 0 ; not overlapped
hex ff 15 35 10 00 00                 # call [rip + 0x1035]      ; WriteFile (0x2068)
hex 31 c9                             # xor ecx, ecx             ; exit code 0
hex ff 15 35 10 00 00                 # call [rip + 0x1035]      ; ExitProcess (0x2070)
hex 0f 0b                             # ud2                      ; never reached
pad_to 0x400

# .rdata, at RVA 0x2000.
# 0x00: the import descriptor -- lookup table, time, forwarders, DLL name,
# address table -- and a zero one to end the list.
u32 0x2040; u32 0; u32 0; u32 0x2080; u32 0x2060
zeros 20
pad_to $((0x400 + 0x40))
# 0x40: the lookup table, and 0x60: the address table, the same before
# binding -- each entry the RVA of a hint and a name -- each ended by zero.
for table in lookup address; do
    u64 0x2090; u64 0x20a0; u64 0x20b0; u64 0
done
# 0x80: the DLL's name; 0x90 on: hint (0) and name of each function.
text "KERNEL32.dll"; pad_to $((0x400 + 0x90))
u16 0; text "GetStdHandle"; pad_to $((0x400 + 0xa0))
u16 0; text "WriteFile"; pad_to $((0x400 + 0xb0))
u16 0; text "ExitProcess"; pad_to $((0x400 + 0x100))
# 0x100: what it writes, 36 bytes, a Windows line ending and all.
text "hello from kernel32, via WriteFile"; hex 0d 0a
pad_to 0x600

printf "$bytes" > "$out"
