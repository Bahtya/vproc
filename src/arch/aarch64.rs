//! FFI binding to the aarch64 assembly context switch routine.

/// Switch coroutine context.
///
/// Saves callee-saved registers to the current stack, stores the current sp
/// into `*old_sp`, then loads sp from `new_sp` and restores registers.
///
/// # Safety
/// Both pointers must point to valid, properly aligned stack memory.
#[inline(always)]
pub unsafe fn context_switch(old_sp: *mut *mut u8, new_sp: *mut u8) {
    extern "C" {
        fn vproc_switch(old_sp: *mut *mut u8, new_sp: *mut u8);
    }
    unsafe { vproc_switch(old_sp, new_sp) }
}
