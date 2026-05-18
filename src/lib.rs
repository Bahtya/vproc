pub mod arch;
pub mod coroutine;
pub mod elf;
pub mod executor;
pub mod ffi;
pub mod loader;
pub mod preload;
pub mod vexec;
pub mod vfd;

use coroutine::VPid;

/// Spawn a new coroutine.
pub fn spawn(f: Box<dyn FnOnce()>) -> VPid {
    unsafe { (*executor::get_global_executor()).spawn(f) }
}

/// Spawn a coroutine at the front of the ready queue (high priority).
pub fn spawn_front(f: Box<dyn FnOnce()>) -> VPid {
    unsafe { (*executor::get_global_executor()).spawn_front(f) }
}

/// Yield control to the next ready coroutine.
pub fn r#yield() {
    executor::do_yield();
}

/// Run all spawned coroutines to completion.
pub fn block_on_all() {
    executor::EXECUTOR.with(|e| unsafe { &mut *e.get() }.block_on_all());
}

/// Get total context switch count.
pub fn switch_count() -> u64 {
    unsafe { (*executor::get_global_executor()).switch_count() }
}

/// Terminate the current coroutine with an exit code.
pub fn exit(code: i32) {
    executor::vproc_exit_with_code(code);
}

/// Get the exit code of a completed coroutine.
pub fn get_exit_code(pid: VPid) -> Option<i32> {
    executor::get_exit_code(pid)
}

/// Convert a C `*const *const c_char` array (NULL-terminated) to `Vec<String>`.
pub(crate) unsafe fn c_array_to_vec(arr: *const *const std::os::raw::c_char) -> Vec<String> {
    let mut vec = Vec::new();
    if arr.is_null() {
        return vec;
    }
    let mut ptr = arr;
    while !(*ptr).is_null() {
        let s = std::ffi::CStr::from_ptr(*ptr)
            .to_string_lossy()
            .into_owned();
        vec.push(s);
        ptr = ptr.add(1);
    }
    vec
}
