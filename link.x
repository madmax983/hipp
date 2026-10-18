/* Minimal Cortex-M layout for the sizing demo (§4.1 gates).
   Not a product linker script — just enough for a link whose
   section sizes can be measured. */
MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 512K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 128K
}
ENTRY(_start)
SECTIONS
{
  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH
  .data : { *(.data .data.*) } > RAM AT > FLASH
  .bss : { *(.bss .bss.*) *(COMMON) } > RAM
}
