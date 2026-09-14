/* Higher-half kernel at -2GB, laid out for the Limine protocol.
 *
 * The `.requests` section must survive linking untouched: Limine scans the
 * loaded image for the request magics, and if the linker garbage-collects them
 * we boot to a black screen with no explanation. KEEP() plus #[used] on the
 * Rust side is what guarantees that.
 *
 * Section boundaries are padded to 4 KiB so each one can carry its own page
 * permissions. Without that, .text and .data share a page and W^X is a lie.
 */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    text    PT_LOAD;
    rodata  PT_LOAD;
    data    PT_LOAD;
}

SECTIONS
{
    /* Top 2GB: matches -C code-model=kernel. */
    . = 0xffffffff80000000;
    __kernel_start = .;

    __text_start = .;
    .text : {
        *(.text .text.*)
    } :text
    . = ALIGN(4096);
    __text_end = .;

    __rodata_start = .;
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    .requests : {
        KEEP(*(.requests))
    } :rodata

    /* Relocation tables. Named explicitly because an unnamed section becomes an
     * ORPHAN: lld places it after everything the script mentions, which puts it
     * past __kernel_end and therefore outside the range init_paging() maps. A
     * stray .got here cost one page fault on the first heap allocation.
     *
     * They are read-only once linked -- nothing writes a GOT entry in a static
     * non-PIE kernel -- so they belong in the rodata segment. scripts/check-image.sh
     * fails the build if a new orphan ever appears. */
    .data.rel.ro : {
        *(.data.rel.ro .data.rel.ro.*)
    } :rodata

    .got : {
        *(.got)
    } :rodata

    .got.plt : {
        *(.got.plt)
    } :rodata

    . = ALIGN(4096);
    __rodata_end = .;

    __data_start = .;
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    . = ALIGN(4096);
    __data_end = .;

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
