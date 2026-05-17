/*
 * aarch64 coroutine context switch.
 *
 * void vproc_switch(void **old_sp, void *new_sp);
 *
 * Saves callee-saved registers to current stack, switches to new stack,
 * then restores registers from the new stack.
 *
 * Callee-saved per AAPCS64: x19-x28, x29(fp), x30(lr), d8-d15
 */

    .text
    .align 2
    .global vproc_switch
    .type vproc_switch, @function

vproc_switch:
    /* Save callee-saved GPRs */
    stp     x19, x20, [sp, #-160]!
    stp     x21, x22, [sp, #16]
    stp     x23, x24, [sp, #32]
    stp     x25, x26, [sp, #48]
    stp     x27, x28, [sp, #64]
    stp     x29, x30, [sp, #80]   /* fp, lr */

    /* Save callee-saved FPRs */
    stp     d8,  d9,  [sp, #96]
    stp     d10, d11, [sp, #112]
    stp     d12, d13, [sp, #128]
    stp     d14, d15, [sp, #144]

    /* Store current sp into *old_sp (x0) */
    mov     x9, sp
    str     x9, [x0]

    /* Load new sp from new_sp (x1) */
    mov     sp, x1

    /* Restore callee-saved FPRs */
    ldp     d14, d15, [sp, #144]
    ldp     d12, d13, [sp, #128]
    ldp     d10, d11, [sp, #112]
    ldp     d8,  d9,  [sp, #96]

    /* Restore callee-saved GPRs */
    ldp     x29, x30, [sp, #80]
    ldp     x27, x28, [sp, #64]
    ldp     x25, x26, [sp, #48]
    ldp     x23, x24, [sp, #32]
    ldp     x21, x22, [sp, #16]
    ldp     x19, x20, [sp], #160

    /* Return to restored lr */
    ret

    .size vproc_switch, . - vproc_switch
