/*
 * test_static_pie.S — minimal aarch64 PIE binary for vproc Phase 3 testing.
 *
 * Uses raw syscalls only (no libc). Writes a message and exits with code 0.
 * When loaded by vproc, the _exit syscall will terminate the real process,
 * so for initial testing we use a write-only version that returns to vproc_exit.
 */

    .text
    .global _start
    .type _start, %function
    .align 2

_start:
    /* write(1, msg, len) */
    mov  x0, #1
    adrp x1, msg
    add  x1, x1, #:lo12:msg
    mov  x2, #24
    mov  x8, #64            /* __NR_write */
    svc  #0

    /* exit(42) — distinctive exit code for testing */
    mov  x0, #42
    mov  x8, #93            /* __NR_exit */
    svc  #0

    .section .rodata
    .align 4
msg:
    .ascii "hello from loaded ELF!\n"    /* 24 bytes */
