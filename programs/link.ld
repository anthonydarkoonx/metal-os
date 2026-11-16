ENTRY(_start)
SECTIONS
{
    . = 0x80000000;
    __base = .;
    /* 32-byte image header read by the kernel's SPAWN: magic, size of the read-execute part, total size, 8 bytes of name */
    .header : { QUAD(0x4D4F5355); QUAD(__text_end - __base); QUAD(__mem_end - __base); KEEP(*(.programname)) }
    .text : { *(.text._start) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) *(.data.rel.ro .data.rel.ro.*) }
    . = ALIGN(4096);
    __text_end = .;
    .data : { *(.data .data.*) *(.got .got.plt) }
    .bss : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(4096);
    __mem_end = .;
    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
