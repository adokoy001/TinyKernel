OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)
SECTIONS {
    . = 0x40000000;
    __code_start = .;
    .code : { KEEP(*(.text.entry)) *(.text .text.*) *(.rodata .rodata.*) }
    __code_end = .;
    . = 0x40001000;
    __data_start = .;
    .data : { *(.data .data.*) *(.got .got.*) }
    __data_end = .;
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) }
    __bss_end = .;
    /DISCARD/ : { *(.eh_frame*) *(.comment*) *(.note*) *(.debug*) }
    ASSERT(_start == 0x40000000, "User entry must be at code start")
    ASSERT(__code_end - __code_start <= 4096, "User code exceeds one RX page")
    ASSERT(__bss_end - __data_start <= 4096, "User data and BSS exceed one RW page")
}
