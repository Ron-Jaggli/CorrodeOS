# fault.exe: a Windows program that faults, for the terminal's `win` to
# report as Windows would (win-test). It says so, then reads address 0:
# an access violation, 0xc0000005.
#
# Built like reader.exe (verdigris/scripts/reader-exe.sh fault.exe fault-exe.S): one
# flat file, every section at the file offset equal to its RVA. It imports
# GetStdHandle and WriteFile from KERNEL32.dll.

        .intel_syntax noprefix
        .text
base:

# --- DOS header, PE signature, COFF header -------------------------------
        .ascii "MZ"
        .org base + 0x3c
        .long pe - base
        .org base + 0x40
pe:     .ascii "PE\0\0"
        .short 0x8664                   # x86-64
        .short 2                        # sections
        .long 0, 0, 0
        .short optional_end - optional  # optional header's size
        .short 0x0023                   # no relocations, executable, large addresses

# --- PE32+ optional header -----------------------------------------------
optional:
        .short 0x20b
        .byte 0, 0
        .long code_end - code           # size of code
        .long 0x1000                    # initialised data
        .long 0                         # uninitialised data
        .long code - base               # entry
        .long code - base               # base of code
        .quad 0x400000                  # image base
        .long 0x1000, 0x1000            # section and file alignment
        .short 6, 0, 0, 0, 6, 0
        .long 0
        .long 0x3000                    # size of image
        .long 0x1000                    # size of headers
        .long 0
        .short 3, 0x0100                # console; NX compatible
        .quad 0x100000, 0x1000, 0x100000, 0x1000
        .long 0, 16
        # Directories: 1 the import table, 12 the import address table.
        .long 0, 0
        .long imports - base, imports_end - imports
        .fill 10, 8, 0
        .long iat - base, iat_end - iat
        .fill 3, 8, 0
optional_end:

# --- section table -------------------------------------------------------
        .ascii ".text\0\0\0"
        .long code_end - code, code - base, 0x1000, code - base, 0, 0
        .short 0, 0
        .long 0x60000020                # code, execute, read
        .ascii ".rdata\0\0"
        .long rdata_end - rdata, rdata - base, 0x1000, rdata - base, 0, 0
        .short 0, 0
        .long 0x40000040                # initialised data, read

# --- .text, at 0x1000 ----------------------------------------------------
        .org base + 0x1000
code:
        # On entry rsp is 8 below a 16-byte boundary.
        sub rsp, 0x38
        mov ecx, -11                    # STD_OUTPUT_HANDLE
        call [rip + GetStdHandle]
        mov rcx, rax
        lea rdx, [rip + said]
        mov r8d, said_end - said
        lea r9, [rsp + 0x30]            # bytes written
        mov qword ptr [rsp + 0x20], 0   # not overlapped
        call [rip + WriteFile]
        mov rax, [0]                    # nothing is mapped there
        int3                            # not reached
code_end:

# --- .rdata, at 0x2000 ---------------------------------------------------
        .org base + 0x2000
rdata:
imports:
        .long lookup - base, 0, 0, dll - base, iat - base
        .fill 5, 4, 0
imports_end:

        .balign 8, 0
lookup:
        .irp name, GetStdHandle, WriteFile
        .quad hint_\name - base
        .endr
        .quad 0
iat:
        .irp name, GetStdHandle, WriteFile
\name:  .quad hint_\name - base
        .endr
        .quad 0
iat_end:

dll:    .asciz "KERNEL32.dll"
        .irp name, GetStdHandle, WriteFile
        .balign 2, 0
hint_\name:
        .short 0
        .asciz "\name"
        .endr

said:   .ascii "reading address 0\n"
said_end:
rdata_end:
