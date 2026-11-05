/* Minimal Cortex-M layout for the sizing demo (§4.1 gates).
   Not a product linker script — just enough for a link whose
   section sizes can be measured. FLASH is larger than any target's:
   the `xl` profile (ADR 0004, budget 1.25 MiB) must link too. */
MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 2048K
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
