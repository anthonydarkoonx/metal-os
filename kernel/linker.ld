/* The kernel is a position-independent executable (ET_DYN) linked at the address QEMU loads it at, 0x40080000 (boot.S
   has the same number). boot.S applies the R_AARCH64_RELATIVE relocations and, in kaslr.rs / boot.S, moves the image to a
   random place in RAM. The image is linked read-write-execute in one piece: RAM is mapped with one set of attributes. */
ENTRY(_start)
SECTIONS
{
    . = 0x40080000;
    __kernel_start = .;
    ASSERT(__kernel_start == 0x40080000, "boot.S assumes the link address 0x40080000")
    .text : { KEEP(*(.text.boot0)) KEEP(*(.text.boot)) *(.text .text.*) }
    .dynsym : { *(.dynsym) }
    .dynstr : { *(.dynstr) }
    .gnu.hash : { *(.gnu.hash) }
    .hash : { *(.hash) }
    .rela.dyn : ALIGN(8) { __rela_start = .; *(.rela.dyn) __rela_end = .; }
    .rodata : ALIGN(8) { *(.rodata .rodata.*) }
    .data.rel.ro : { *(.data.rel.ro .data.rel.ro.*) }
    .dynamic : { *(.dynamic) }
    .got : { *(.got .got.plt) }
    .data : ALIGN(8) { *(.data .data.*) }
    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }
    . = ALIGN(4096);
    __stack_bottom = .;
    . += 0x50000;
    __stack_top = .;
    __kernel_end = .;
    __image_size = ABSOLUTE(__kernel_end - __kernel_start);
    /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
