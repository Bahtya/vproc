pub mod arch;
pub mod coroutine;
pub mod elf;
pub mod executor;
pub mod loader;
pub mod preload;
pub mod vexec;

use coroutine::VPid;

/// Spawn a new coroutine.
pub fn spawn(f: Box<dyn FnOnce()>) -> VPid {
    executor::EXECUTOR.with(|e| unsafe { &mut *e.get() }.spawn(f))
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
    executor::EXECUTOR.with(|e| unsafe { &mut *e.get() }.switch_count())
}
