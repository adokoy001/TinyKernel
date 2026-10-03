OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)
SECTIONS
{
    . = 0x10000;
    .text : { KEEP(*(.text.entry)) *(.text .text.*) }
    .rodata ALIGN(16) : { *(.rodata .rodata.*) }
    .data ALIGN(16) : { *(.data .data.*) *(.got .got.*) }
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
