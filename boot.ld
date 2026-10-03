OUTPUT_FORMAT(elf64-x86-64)
ENTRY(boot_start)
SECTIONS
{
    . = 0x7c00;
    .boot : { KEEP(*(.boot)) }
    /DISCARD/ : { *(.note*) *(.comment*) *(.eh_frame*) }
}
ASSERT(SIZEOF(.boot) == 512, "Boot sector must be exactly 512 bytes")
