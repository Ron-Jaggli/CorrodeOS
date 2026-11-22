# reader.exe: Verdigris's third phase, a Windows program that reads files.
#
# Assembled by GNU as into one flat file (scripts/reader-exe.sh): every
# section is placed at a file offset equal to its RVA (file alignment
# 0x1000), so the assembler itself resolves every RVA and every
# RIP-relative address. No Windows toolchain, no linker.
#
# It imports CreateFileW, ReadFile, GetFileSize, CloseHandle, GetLastError,
# FindFirstFileW, FindNextFileW, FindClose, GetStdHandle, WriteFile and
# ExitProcess from KERNEL32.dll. For each path in its first table it opens
# the file, reads it, and writes the file's first line to standard output;
# for each in its second it searches, and writes each name found (a
# directory's with a `\` after it). When a call fails it says which error
# GetLastError gave. The kernel starts it in /docs, allowed /etc/motd
# besides, so what it may see and what it may not are both on the lists.

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
        .short 3                        # sections
        .long 0, 0, 0
        .short optional_end - optional  # optional header's size
        .short 0x0023                   # no relocations, executable, large addresses

# --- PE32+ optional header -----------------------------------------------
optional:
        .short 0x20b
        .byte 0, 0
        .long code_end - code           # size of code
        .long 0x1000                    # initialised data
        .long 0x2000                    # uninitialised data
        .long code - base               # entry
        .long code - base               # base of code
        .quad 0x400000                  # image base
        .long 0x1000, 0x1000            # section and file alignment
        .short 6, 0, 0, 0, 6, 0
        .long 0
        .long 0x5000                    # size of image
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
        .ascii ".bss\0\0\0\0"
        .long 0x2000, 0x3000, 0, 0, 0, 0
        .short 0, 0
        .long 0xc0000080                # uninitialised data, read, write

# --- .text, at 0x1000 ----------------------------------------------------
        .org base + 0x1000
        .set buffer, base + 0x3000      # 4 KiB for a read, then 4 KiB more
        .set found, buffer              # or a WIN32_FIND_DATAW,
        .set line, buffer + 1024        # and a name to write

code:
        # On entry rsp is 8 below a 16-byte boundary.
        sub rsp, 0x28
        .irp n, 1, 2, 3, 4, 5, 6, 7, 8, 9
        lea rcx, [rip + say_\n]
        mov edx, say_\n\()_end - say_\n
        call put
        lea rcx, [rip + path_\n]
        mov edx, [rip + access_\n]
        call show
        .endr
        .irp n, 10, 11, 12, 13, 14, 15
        lea rcx, [rip + path_\n]
        lea rdx, [rip + say_\n]
        mov r8d, say_\n\()_end - say_\n
        call list
        .endr
        xor ecx, ecx
        call [rip + ExitProcess]
        ud2

# put(rcx = text, rdx = its length): to standard output.
put:
        push rbx
        push rsi
        sub rsp, 0x38                   # 8 + 16 + 0x38: aligned for calls
        mov rbx, rcx
        mov rsi, rdx
        mov ecx, -11                    # STD_OUTPUT_HANDLE
        call [rip + GetStdHandle]
        mov rcx, rax
        mov rdx, rbx
        mov r8, rsi
        lea r9, [rsp + 0x30]            # bytes written
        mov qword ptr [rsp + 0x20], 0   # not overlapped
        call [rip + WriteFile]
        add rsp, 0x38
        pop rsi
        pop rbx
        ret

# show(rcx = UTF-16 path, edx = access): open it; its first line, and
# whether GetFileSize and a second, empty ReadFile agree with the first --
# or the error.
show:
        push rbx
        push rsi
        push rdi
        push r12
        sub rsp, 0x48                   # 8 + 32 + 0x48: aligned for calls
        mov r8d, 1                      # FILE_SHARE_READ
        xor r9d, r9d                    # no security attributes
        mov qword ptr [rsp + 0x20], 3   # OPEN_EXISTING
        mov qword ptr [rsp + 0x28], 0   # no flags
        mov qword ptr [rsp + 0x30], 0   # no template
        call [rip + CreateFileW]
        cmp rax, -1
        je failed
        mov rbx, rax

        mov rcx, rbx                    # ReadFile(handle, buffer, 4096, &read, 0)
        lea rdx, [rip + buffer]
        mov r8d, 4096
        lea r9, [rsp + 0x38]
        mov qword ptr [rsp + 0x20], 0
        call [rip + ReadFile]
        test eax, eax
        jz failed_closing
        mov esi, [rsp + 0x38]           # bytes read

        mov rcx, rbx                    # and again: at the end, nothing
        lea rdx, [rip + buffer + 4096]
        mov r8d, 4096
        lea r9, [rsp + 0x38]
        mov qword ptr [rsp + 0x20], 0
        call [rip + ReadFile]
        mov r12d, [rsp + 0x38]

        mov rcx, rbx                    # GetFileSize(handle, 0)
        xor edx, edx
        call [rip + GetFileSize]
        mov edi, eax

        mov rcx, rbx
        call [rip + CloseHandle]

        lea rcx, [rip + buffer]         # the first line, its end included
        mov rdx, rsi
        xor eax, eax
first_line:
        cmp rax, rdx
        je line_found
        inc rax
        cmp byte ptr [rcx + rax - 1], 10
        jne first_line
line_found:
        mov rdx, rax
        call put

        cmp edi, esi                    # the size, and the empty second read
        jne differs
        test r12d, r12d
        jnz differs
        lea rcx, [rip + agrees]
        mov edx, agrees_end - agrees
        jmp said
differs:
        lea rcx, [rip + disagrees]
        mov edx, disagrees_end - disagrees
        jmp said

failed_closing:
        mov rcx, rbx
        call [rip + CloseHandle]
failed:
        call [rip + GetLastError]
        .irp code, 2, 3, 5, 18, 19
        cmp eax, \code
        jne 1f
        lea rcx, [rip + error_\code]
        mov edx, error_\code\()_end - error_\code
        jmp said
1:
        .endr
        lea rcx, [rip + error_other]
        mov edx, error_other_end - error_other
said:
        call put
        add rsp, 0x48
        pop r12
        pop rdi
        pop rsi
        pop rbx
        ret

# list(rcx = UTF-16 pattern, rdx = what to say first, r8 = its length):
# each name a search finds, a line each, then why it ended. Its frame is
# show's, so it ends through show's error path.
list:
        push rbx
        push rsi
        push rdi
        push r12
        sub rsp, 0x48
        mov rsi, rdx
        mov rdi, r8
        lea rdx, [rip + found]
        call [rip + FindFirstFileW]
        cmp rax, -1
        je list_failed
        mov r12, rax
list_next:
        mov rcx, rsi
        mov rdx, rdi
        call put
        lea rcx, [rip + found + 44]     # the name, UTF-16, to bytes
        lea rdx, [rip + line]
        xor eax, eax
list_copy:
        movzx r8d, word ptr [rcx + rax * 2]
        test r8d, r8d
        jz list_copied
        mov [rdx + rax], r8b
        inc rax
        jmp list_copy
list_copied:
        test dword ptr [rip + found], 0x10   # FILE_ATTRIBUTE_DIRECTORY
        jz 1f
        mov byte ptr [rdx + rax], 0x5c
        inc rax
1:      mov byte ptr [rdx + rax], 10
        inc rax
        mov rcx, rdx
        mov rdx, rax
        call put
        mov rcx, r12
        lea rdx, [rip + found]
        call [rip + FindNextFileW]
        test eax, eax
        jnz list_next
        mov rcx, r12                    # the end: say why, as for a failure
        call [rip + FindClose]
list_failed:
        mov rcx, rsi
        mov rdx, rdi
        call put
        jmp failed
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
        .irp name, CreateFileW, ReadFile, GetFileSize, CloseHandle, GetLastError, FindFirstFileW, FindNextFileW, FindClose, GetStdHandle, WriteFile, ExitProcess
        .quad hint_\name - base
        .endr
        .quad 0
iat:
        .irp name, CreateFileW, ReadFile, GetFileSize, CloseHandle, GetLastError, FindFirstFileW, FindNextFileW, FindClose, GetStdHandle, WriteFile, ExitProcess
\name:  .quad hint_\name - base
        .endr
        .quad 0
iat_end:

dll:    .asciz "KERNEL32.dll"
        .irp name, CreateFileW, ReadFile, GetFileSize, CloseHandle, GetLastError, FindFirstFileW, FindNextFileW, FindClose, GetStdHandle, WriteFile, ExitProcess
        .balign 2, 0
hint_\name:
        .short 0
        .asciz "\name"
        .endr

# The cases: what it says first, the path, and the access asked for.
        .macro case n, say, path, access=0x80000000
say_\n: .ascii "\say: "
say_\n\()_end:
        .balign 2, 0
path_\n:
        .string16 "\path"
        .short 0
        .balign 4, 0
access_\n:
        .long \access
        .endm

        case 1, "keys.txt", "keys.txt"
        case 2, "c:/Etc/MOTD", "c:/Etc/MOTD"
        case 3, "..\\..\\etc\\motd", "..\\..\\etc\\motd"
        case 4, "C:\\etc\\desktop.conf", "C:\\etc\\desktop.conf"
        case 5, "C:\\etc\\nothing.txt", "C:\\etc\\nothing.txt"
        case 6, "C:\\home\\hello.txt", "C:\\home\\hello.txt"
        case 7, "C:\\etc\\motd, to write", "C:\\etc\\motd", 0x40000000
        case 8, "D:\\etc\\motd", "D:\\etc\\motd"
        case 9, "C:\\docs", "C:\\docs"

        .macro listing n, say, path
say_\n: .ascii "\say: "
say_\n\()_end:
        .balign 2, 0
path_\n:
        .string16 "\path"
        .short 0
        .endm

        listing 10, "C:\\*", "C:\\*"
        listing 11, "C:\\etc\\*", "C:\\etc\\*"
        listing 12, "c:/docs/*.TXT", "c:/docs/*.TXT"
        listing 13, "C:\\etc\\m?td", "C:\\etc\\m?td"
        listing 14, "C:\\etc\\desktop.c*", "C:\\etc\\desktop.c*"
        listing 15, "C:\\home\\*", "C:\\home\\*"

agrees: .ascii "GetFileSize and the next ReadFile agree\n"
agrees_end:
disagrees:
        .ascii "GetFileSize or the next ReadFile disagrees\n"
disagrees_end:
error_2: .ascii "not there (2, ERROR_FILE_NOT_FOUND)\n"
error_2_end:
error_3: .ascii "not there (3, ERROR_PATH_NOT_FOUND)\n"
error_3_end:
error_5: .ascii "refused (5, ERROR_ACCESS_DENIED)\n"
error_5_end:
error_18: .ascii "no more (18, ERROR_NO_MORE_FILES)\n"
error_18_end:
error_19: .ascii "read-only (19, ERROR_WRITE_PROTECT)\n"
error_19_end:
error_other: .ascii "failed some other way\n"
error_other_end:
rdata_end:
        .org base + 0x3000
