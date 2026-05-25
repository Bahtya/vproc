pub mod coroutine;
pub mod elf;
pub mod executor;
pub mod ffi;
pub mod loader;
pub mod log;
// LD_PRELOAD hooks take raw C pointers by design — suppress the lint for the whole module.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub mod preload;
pub mod vexec;
pub mod vfd;

pub const VERSION: &str = "0.4.0";

use coroutine::VPid;

/// Spawn a new coroutine.
pub fn spawn(f: Box<dyn FnOnce()>) -> VPid {
    unsafe { (*executor::get_current_executor()).spawn(f) }
}

/// Spawn a coroutine at the front of the ready queue (high priority).
pub fn spawn_front(f: Box<dyn FnOnce()>) -> VPid {
    unsafe { (*executor::get_current_executor()).spawn_front(f) }
}

/// Yield control to the next ready coroutine.
pub fn r#yield() {
    executor::do_yield();
}

/// Run all spawned coroutines to completion.
/// Note: with per-session architecture, this is no longer used externally.
pub fn block_on_all() {
    // Deprecated — session driver loop handles scheduling
}

/// Get total context switch count.
pub fn switch_count() -> u64 {
    unsafe { (*executor::get_current_executor()).switch_count() }
}

/// Terminate the current coroutine with an exit code.
pub fn exit(code: i32) {
    executor::vproc_exit_with_code(code);
}

/// Get the exit code of a completed coroutine.
pub fn get_exit_code(pid: VPid) -> Option<i32> {
    executor::get_exit_code(pid)
}

/// Remove a binary from the cache and dlclose its handle.
pub fn unload_binary(path: &str) -> bool {
    vexec::unload_binary(path)
}

/// Remove all binaries from the cache and dlclose their handles.
pub fn unload_all_binaries() {
    vexec::unload_all_binaries()
}

/// Return the number of cached binaries.
pub fn cached_binary_count() -> usize {
    vexec::cached_binary_count()
}

/// Clean up global fd tables. Call after all coroutines have finished.
pub fn cleanup_fd_tables() {
    vfd::cleanup();
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
