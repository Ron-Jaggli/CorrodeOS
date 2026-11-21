/* Verdigris's kernel32, laid out for the kernel's Windows loader.
 *
 * As user/user.ld, but at KERNEL32_BASE (corrode_abi::verdigris), above
 * every Windows program's image, and with one more segment first: the
 * export table, alone at the base, where the kernel looks for it.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    exports PT_LOAD FLAGS(4);
    text    PT_LOAD FLAGS(5);
    rodata  PT_LOAD FLAGS(4);
    data    PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x8000000;

    .exports : {
        KEEP(*(.verdigris.exports))
    } :exports

    . = ALIGN(4096);
    .text : {
        *(.text._start)
        *(.text .text.*)
    } :text

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    .data.rel.ro : {
        *(.data.rel.ro .data.rel.ro.*)
    } :rodata

    .got : {
        *(.got .got.plt)
    } :rodata

    . = ALIGN(4096);
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
