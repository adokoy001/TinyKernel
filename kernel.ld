OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)
SECTIONS
{
    . = 0x10000;
    /* Page-aligned so paging can map code read-only + executable and
     * everything else non-executable (see src/paging.rs). */
    __text_start = .;
    .text : { KEEP(*(.text.entry)) *(.text .text.*) }
    . = ALIGN(4096);
    __text_end = .;
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    __rodata_end = .;
    .data : { *(.data .data.*) *(.got .got.*) }
    .bss ALIGN(16) (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        __bss_end = .;
    }
    __kernel_end = .;
    /DISCARD/ : { *(.eh_frame*) *(.comment*) *(.note*) *(.debug*) }
    ASSERT(_start == 0x10000, "Kernel entry must be at 0x10000")
    ASSERT(__kernel_end <= 0x80000, "Kernel overlaps reserved stack")
}
