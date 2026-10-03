/*
 * Tane OS: original BIOS boot sector, no external bootloader.
 * GNU assembler, linked at 0x7c00. Boot as a 1.44 MiB floppy.
 * LBA 1..512 are copied to 0x10000..0x4ffff, then called in long mode.
 * The loader deliberately assumes the floppy geometry: 18 sectors, 2 heads.
 */
.intel_syntax noprefix
/* build.py reads this literal too; the loader and image limit stay in sync. */
.equ KERNEL_SECTORS, 512
.section .boot, "ax"
.global boot_start
.code16
boot_start:
    /* Normalize CS because a BIOS may enter at either 0000:7c00 or 07c0:0. */
    .byte 0xea
    .word real_start
    .word 0
real_start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00
    cld
    /* DX keeps DL = BIOS boot drive and DH = floppy head. */
    xor dh, dh
    mov ax, 0x1000
    mov es, ax
    xor bx, bx
    mov cx, 2                 /* cylinder 0, sector 2 (LBA 1) */
    mov si, KERNEL_SECTORS
    sti
read_sector:
    mov bp, 3
read_retry:
    push ds
    push es
    pusha
    mov ax, 0x0201            /* BIOS read exactly one sector */
    stc
    int 0x13
    popa
    pop es
    pop ds
    jnc read_ok
    /* Reset the same drive and retry; preserve CHS and destination. */
    push ds
    push es
    pusha
    xor ah, ah
    int 0x13
    popa
    pop es
    pop ds
    dec bp
    jnz read_retry
    mov ax, 0x0e45            /* E = disk read error */
    xor bx, bx
    int 0x10
    cli
disk_error:
    hlt
    jmp disk_error
read_ok:
    /* Sectors are 512-byte aligned, so none crosses a 64 KiB DMA boundary;
     * after each full 64 KiB window, move ES to the next one. */
    add bx, 512
    jnz same_window
    mov ax, es
    add ax, 0x1000
    mov es, ax
same_window:
    inc cl
    cmp cl, 19
    jb next_sector
    mov cl, 1
    xor dh, 1
    jnz next_sector
    inc ch
next_sector:
    dec si
    jnz read_sector

    /* BIOS INT 15h E820 memory map: entry count (u16) at 0x5000 and up to
     * 64 entries of 24 bytes from 0x5010. A 20-byte reply keeps the preset
     * ACPI 3.0 attribute "valid". The kernel reads this table later. */
    xor ax, ax
    mov es, ax
    mov word ptr [0x5000], ax
    mov di, 0x5010
    xor ebx, ebx
e820_next:
    mov dword ptr [di + 20], 1
    mov eax, 0xe820
    mov ecx, 24
    mov edx, 0x534d4150       /* "SMAP" */
    int 0x15
    jc e820_done              /* unsupported, or the end on some BIOSes */
    cmp eax, 0x534d4150
    jne e820_done
    jcxz e820_skip
    add di, 24
    inc word ptr [0x5000]
    cmp word ptr [0x5000], 64
    jae e820_done
e820_skip:
    test ebx, ebx
    jnz e820_next
e820_done:

    cli
    /* Fast A20 gate: enable A20 and keep the fast-reset bit clear. */
    in al, 0x92
    and al, 0xfe
    or al, 2
    out 0x92, al
    lgdt [gdt_pointer]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    /* Operand-size override: 16-bit code to a 32-bit far destination. */
    .byte 0x66, 0xea
    .long protected_start
    .word 0x08

.code32
protected_start:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
    cld

    /* Three zeroed 4 KiB tables: PML4 -> PDPT -> 512 large pages. */
    xor eax, eax
    mov edi, 0x1000
    mov ecx, 3072
    rep stosd
    mov dword ptr [0x1000], 0x2003
    mov dword ptr [0x2000], 0x3003
    mov edi, 0x3000
    mov eax, 0x83             /* present | writable | 2 MiB page */
    mov ecx, 512
map_page:
    mov [edi], eax
    add eax, 0x200000
    add edi, 8
    loop map_page

    /* PAE plus SSE context support; no AVX instructions are required. */
    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax
    mov eax, 0x1000
    mov cr3, eax
    mov ecx, 0xc0000080       /* IA32_EFER */
    rdmsr
    or eax, 1 << 8            /* LME */
    wrmsr
    mov eax, cr0
    and eax, 0xfffffff3       /* clear EM and TS */
    or eax, 0x80000003        /* paging | protected mode | monitor copro. */
    mov cr0, eax
    fninit
    ldmxcsr [mxcsr_default]
    .byte 0xea
    .long long_start
    .word 0x18

.code64
long_start:
    xor eax, eax
    mov fs, ax
    mov gs, ax
    mov rsp, 0x90000
    xor ebp, ebp
    mov eax, 0x10000
    call rax                 /* SysV entry stack: RSP % 16 == 8 */
    cli
kernel_returned:
    hlt
    jmp kernel_returned

.balign 8
gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff   /* 0x08: 32-bit ring 0 code */
    .quad 0x00cf92000000ffff   /* 0x10: ring 0 data */
    .quad 0x00af9a000000ffff   /* 0x18: 64-bit ring 0 code */
gdt_end:
gdt_pointer:
    .word gdt_end - gdt - 1
    .long gdt
mxcsr_default:
    .long 0x1f80

/* A BIOS disk boot sector is exactly 512 bytes with this final signature. */
.org 510
    .word 0xaa55
.section .note.GNU-stack,"",@progbits
