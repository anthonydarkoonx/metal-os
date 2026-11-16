/* The boot-time programs: one position-independent ELF file (ET_DYN, linked at 0). The kernel's loader puts it at a random
   base and applies the relative relocations. Read-only and writable parts are on pages of their own. */
ENTRY(_start)
SECTIONS
{
    . = 0;
    .dynsym : { *(.dynsym) }
    .dynstr : { *(.dynstr) }
    .gnu.hash : { *(.gnu.hash) }
    .hash : { *(.hash) }
    .rela.dyn : { *(.rela.dyn) }
    .text : { *(.text._start) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    .data.rel.ro : { *(.data.rel.ro .data.rel.ro.*) }
    .dynamic : { *(.dynamic) }
    .got : { *(.got .got.plt) }
    .data : { *(.data .data.*) }
    .bss : { *(.bss .bss.*) *(COMMON) }
    . = ALIGN(4096);
    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
