/*
 * __vproc_elf_entry — trampoline for entering a loaded ELF binary.
 *
 * When vproc_switch restores a frame where x30 = this address,
 * we set sp to the ELF stack layout and jump to the entry point.
 *
 * Register state at entry:
 *   x19 = ELF entry point address
 *   x20 = sp pointing to argc/argv/envp/auxv stack layout
 */

    .text
    .align 2
    .global __vproc_elf_entry
    .type __vproc_elf_entry, @function

__vproc_elf_entry:
    mov  sp, x20       // switch to the ELF stack layout
    br   x19           // jump to ELF entry point (no return)

    .size __vproc_elf_entry, . - __vproc_elf_entry
