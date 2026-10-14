/* A ring 3 program, laid out for Corrode's loader.
 *
 * Fixed addresses from 4 MiB up, no relocations, no dynamic section. Three
 * loadable segments, each starting on a page of its own, because the loader
 * gives a page one set of permissions and refuses a file that would need two:
 * text is read and execute, rodata is read, data and bss are read and write.
 * Nothing is ever writable and executable.
 *
 * Every allocatable section is named. An orphan would be placed by the
 * linker wherever it fits, which may be in the wrong segment.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(5);
    rodata  PT_LOAD FLAGS(4);
    data    PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x400000;

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
