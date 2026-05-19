//! C FFI interface for the vproc runtime.
//!
//! Exported by libvproc.so, called by the C preload layer (libvproc_preload.so).

use std::os::raw::{c_char, c_int, c_void};

/// Returns the current virtual process ID, or 0 if not in a coroutine.
#[no_mangle]
pub extern "C" fn vproc_ffi_current_vpid() -> u32 {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return 0;
    }
    unsafe { (*ptr).current.unwrap_or(0) }
}

/// Exit the current virtual process. Does not return.
/// If not in a coroutine context, uses raw syscall to avoid interceptor recursion.
#[no_mangle]
pub extern "C" fn vproc_ffi_exit(code: c_int) {
    let in_coroutine = crate::executor::EXECUTOR.with(|e| unsafe { (*e.get()).current.is_some() });
    if in_coroutine {
        crate::executor::vproc_exit_with_code(code);
    } else {
        // Raw syscall — bypass LD_PRELOAD interceptors to avoid recursion.
        unsafe {
            std::arch::asm!(
                "mov x8, #94", // __NR_exit_group on aarch64
                "svc #0",
                in("x0") code,
                options(noreturn)
            );
        }
    }
}

/// Yield control to the next ready virtual process.
#[no_mangle]
pub extern "C" fn vproc_ffi_yield() {
    crate::executor::do_yield();
}

/// Get exit code of a completed virtual process. Returns -1 if not done yet.
#[no_mangle]
pub extern "C" fn vproc_ffi_get_exit_code(vpid: u32) -> c_int {
    match crate::executor::get_exit_code(vpid) {
        Some(code) => code,
        None => -1,
    }
}

/// Check if a virtual process exists.
#[no_mangle]
pub extern "C" fn vproc_ffi_vpid_exists(vpid: u32) -> c_int {
    crate::executor::EXECUTOR.with(|e| unsafe {
        if (*e.get()).vprocs.contains_key(&vpid) {
            1
        } else {
            0
        }
    })
}

/// Create a virtual pipe. Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn vproc_ffi_pipe(vpid: u32, fds: *mut c_int) -> c_int {
    let table = crate::vfd::get_or_create_table(vpid);
    let (read_fd, write_fd) = table.create_pipe();
    unsafe {
        *fds = read_fd as c_int;
        *fds.add(1) = write_fd as c_int;
    }
    0
}

/// Check if fd is virtual (not a real kernel fd).
#[no_mangle]
pub extern "C" fn vproc_ffi_is_virtual_fd(vpid: u32, fd: c_int) -> c_int {
    crate::vfd::get_table(vpid)
        .and_then(|t| t.get(fd as u32))
        .map(|vfd| match vfd {
            crate::vfd::Vfd::Real(_) => 0,
            _ => 1,
        })
        .unwrap_or(0)
}

/// Read from a virtual pipe fd. Returns bytes read, or -1 (EAGAIN if empty).
#[no_mangle]
pub extern "C" fn vproc_ffi_read(
    vpid: u32,
    fd: c_int,
    buf: *mut c_void,
    count: usize,
) -> isize {
    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => return -1,
    };
    match table.get(fd as u32) {
        Some(crate::vfd::Vfd::PipeRead(pipe_buf)) => {
            let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count) };
            pipe_buf.read_from(dst)
        }
        _ => -1,
    }
}

/// Write to a virtual pipe fd. Returns bytes written, or -1 (EAGAIN if full).
#[no_mangle]
pub extern "C" fn vproc_ffi_write(
    vpid: u32,
    fd: c_int,
    buf: *const c_void,
    count: usize,
) -> isize {
    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => return -1,
    };
    match table.get(fd as u32) {
        Some(crate::vfd::Vfd::PipeWrite(pipe_buf)) => {
            let src = unsafe { std::slice::from_raw_parts(buf as *const u8, count) };
            pipe_buf.write_to(src)
        }
        _ => -1,
    }
}

/// Check if a pipe write end is closed.
#[no_mangle]
pub extern "C" fn vproc_ffi_pipe_is_closed(vpid: u32, fd: c_int) -> c_int {
    crate::vfd::get_table(vpid)
        .and_then(|t| t.get(fd as u32))
        .map(|vfd| match vfd {
            crate::vfd::Vfd::PipeWrite(buf) => {
                if buf.is_closed() {
                    1
                } else {
                    0
                }
            },
            _ => 0,
        })
        .unwrap_or(0)
}

/// Close a virtual fd. Returns 0 on success, -1 on error.
#[no_mangle]
pub extern "C" fn vproc_ffi_close(vpid: u32, fd: c_int) -> c_int {
    match crate::vfd::get_table(vpid) {
        Some(t) => {
            match t.get(fd as u32) {
                Some(crate::vfd::Vfd::File(_)) => {
                    if let Some(real_fd) = t.close_file_fd(fd as u32) {
                        unsafe { crate::preload::real_close(real_fd); }
                    }
                    0
                }
                _ => match t.close(fd as u32) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            }
        }
        None => -1,
    }
}

/// Duplicate a virtual fd.
#[no_mangle]
pub extern "C" fn vproc_ffi_dup(vpid: u32, old_fd: c_int) -> c_int {
    match crate::vfd::get_table(vpid) {
        Some(t) => match t.dup(old_fd as u32) {
            Ok(fd) => fd as c_int,
            Err(_) => -1,
        },
        None => -1,
    }
}

/// Duplicate a virtual fd to a specific fd number.
#[no_mangle]
pub extern "C" fn vproc_ffi_dup2(vpid: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    match crate::vfd::get_table(vpid) {
        Some(t) => match t.dup2(old_fd as u32, new_fd as u32) {
            Ok(fd) => fd as c_int,
            Err(_) => -1,
        },
        None => -1,
    }
}

/// Get the current virtual process ID. Returns real PID if not in a coroutine.
#[no_mangle]
pub extern "C" fn vproc_ffi_getpid() -> u32 {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return unsafe { libc::getpid() as u32 };
    }
    unsafe {
        match (*ptr).current {
            Some(pid) => pid,
            None => libc::getpid() as u32,
        }
    }
}

/// Get the parent virtual process ID. Returns real PPID if not in a coroutine.
#[no_mangle]
pub extern "C" fn vproc_ffi_getppid() -> u32 {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return unsafe { libc::getppid() as u32 };
    }
    unsafe {
        let ex = &*ptr;
        match ex.current {
            Some(pid) => ex
                .vprocs
                .get(&pid)
                .map(|co| co.ppid)
                .unwrap_or(0),
            None => libc::getppid() as u32,
        }
    }
}

/// Virtual execve — load and execute an ELF binary in a coroutine.
/// On success, terminates the calling coroutine (does not return).
/// Returns -1 on error.
#[no_mangle]
pub extern "C" fn vproc_ffi_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let path_str = unsafe { std::ffi::CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    let argv_vec = unsafe { crate::c_array_to_vec(argv) };
    let envp_vec = unsafe { crate::c_array_to_vec(envp) };

    match crate::vexec::virtual_execve(&path_str, argv_vec, envp_vec) {
        Ok(_) => {
            crate::executor::vproc_exit_with_code(0);
            -1 // unreachable but needed for type
        }
        Err(e) => {
            let msg = format!("vproc: virtual_execve: {}\n", e);
            unsafe { libc::syscall(64, 2, msg.as_ptr(), msg.len()); }
            unsafe { *libc::__errno() = libc::ENOEXEC };
            -1
        }
    }
}

/// Perform virtual fork.
///
/// Returns child's VPid to the parent. The child (when scheduled later)
/// returns 0 from this function.
///
/// Mechanism: spawn a tiny helper coroutine, yield to it. The helper
/// copies the parent's stack (including the saved register frame that
/// vproc_switch just wrote), creates the child coroutine, and exits.
/// The parent resumes and reads the child pid. The child, when scheduled,
/// resumes inside this function after do_yield() and returns 0 via
/// fork_child_pid (initialized to 0 by fork_from).
///
/// Virtual fork — create a child coroutine by copying the parent's stack.
///
/// **Deprecated:** preload.rs fork() now uses real OS fork for memory isolation.
/// This function is retained for the FFI API but is no longer called from the
/// main interception path.
///
/// # Safety limitation
///
/// vproc_switch only saves callee-saved registers (x19-x30, d8-d15).
/// Variables in caller-saved registers (x0-x18) at the fork point are
/// NOT preserved in the child. Code between fork() and execve() must
/// not rely on caller-saved register values. For the fork-then-execve
/// pattern (the primary use case), this is safe because the child only
/// reads the fork return value (x0 = 0) before calling execve.
#[no_mangle]
pub extern "C" fn vproc_ffi_fork() -> u32 {
    // Phase 1: check if we are a fork child returning from a call path
    // that entered vproc_ffi_fork directly (not via do_yield resume).
    let ex = unsafe { &mut *crate::executor::get_global_executor() };
    let fork_result = if let Some(pid) = ex.current {
        let co = ex.vprocs.get(&pid).unwrap();
        if co.is_fork_child {
            ex.vprocs.get_mut(&pid).unwrap().is_fork_child = false;
            0u32
        } else {
            u32::MAX
        }
    } else {
        u32::MAX
    };

    if fork_result != u32::MAX {
        return fork_result;
    }

    // Phase 2 (parent): spawn helper at front of queue to copy our stack.
    // Using spawn_front ensures the helper runs before any other coroutine,
    // minimizing the window where other coroutines can corrupt our stack.
    let parent_pid = ex.current.unwrap();

    // Save lr directly from x30 register to the Executor (heap-allocated,
    // immune to stack corruption from dlopen/__libc_init in other coroutines).
    unsafe {
        let lr_value: u64;
        std::arch::asm!("mov {}, x30", out(reg) lr_value);
        (*crate::executor::get_global_executor()).saved_fork_lr = Some(lr_value);
    }

    crate::spawn_front(Box::new(move || {
        unsafe {
            (*crate::executor::get_global_executor()).spawn_fork_child(parent_pid);
        }
    }));

    crate::executor::do_yield();

    // Phase 3: restore lr if corrupted during yield by dlopen/__libc_init.
    unsafe {
        if let Some(lr_val) = (*crate::executor::get_global_executor()).saved_fork_lr.take() {
            let current_lr: u64;
            std::arch::asm!("mov {}, x30", out(reg) current_lr);
            if current_lr != lr_val {
                std::arch::asm!("mov x30, {}", in(reg) lr_val);
            }
        }
    }

    // Fork children also resume here (their stack was copied at do_yield).
    // Distinguish by checking is_fork_child which was set by spawn_fork_child.
    let ex = unsafe { &mut *crate::executor::get_global_executor() };
    let pid = ex.current.unwrap();
    let co = ex.vprocs.get(&pid).unwrap();
    if co.is_fork_child {
        ex.vprocs.get_mut(&pid).unwrap().is_fork_child = false;
        return 0;
    }

    // Parent: read child pid from our Coroutine.
    ex.vprocs.get(&pid).unwrap().fork_child_pid
}

// ---------------------------------------------------------------------------
// High-level process creation (for hermux integration)
// ---------------------------------------------------------------------------

static EXECUTOR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Create a virtual process: load binary in coroutine, map fd 0/1/2 to real fds.
///
/// Returns virtual PID (> 0) on success, 0 on error.
#[no_mangle]
pub extern "C" fn vproc_ffi_create_process(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    stdin_fd: c_int,
    stdout_fd: c_int,
    stderr_fd: c_int,
) -> u32 {
    let path_str = match unsafe { std::ffi::CStr::from_ptr(path) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return 0,
    };
    let argv_vec = unsafe { crate::c_array_to_vec(argv) };
    let envp_vec = unsafe { crate::c_array_to_vec(envp) };

    match crate::vexec::virtual_execve_via_entry(&path_str, argv_vec, envp_vec) {
        Ok(exec) => {
            // Replace the default fd table with custom mappings to PTY slave
            crate::vfd::create_table_with_fds(exec.vpid, stdin_fd, stdout_fd, stderr_fd);
            exec.vpid
        }
        Err(e) => {
            let msg = format!("vproc_ffi_create_process: {}\n", e);
            unsafe { libc::syscall(64, 2, msg.as_ptr(), msg.len()); }
            0
        }
    }
}

/// Drive the scheduler until the given vpid exits.
/// Returns the exit code. Blocks the calling thread.
///
/// Uses a dedicated driver thread to call do_yield(), because the caller
/// (Java waitFor thread) is not a coroutine context. do_yield() saves the
/// caller's stack pointer as main_sp — if multiple Java threads call it
/// concurrently they corrupt each other's main_sp. The driver thread is
/// the single "main" context that drives all coroutines safely.
///
/// Completion is signaled via a Condvar so the calling thread just blocks
/// without touching the scheduler.
#[no_mangle]
pub extern "C" fn vproc_ffi_run_until_exit(vpid: u32) -> c_int {
    // Fast path: already done
    {
        let _guard = EXECUTOR_MUTEX.lock().unwrap();
        if let Some(code) = crate::executor::get_exit_code(vpid) {
            return code;
        }
        let ptr = crate::executor::get_global_executor();
        if ptr.is_null() || !unsafe { (*ptr).vprocs.contains_key(&vpid) } {
            return -1;
        }
    }

    // Shared state between this thread and the driver
    let result = std::sync::Arc::new((
        std::sync::Mutex::new(None::<i32>),
        std::sync::Condvar::new(),
    ));

    // Register with the driver
    WAITERS.lock().unwrap().push(Waiter {
        vpid,
        result: std::sync::Arc::clone(&result),
    });

    // Ensure the driver thread is running
    start_driver_once();

    // Block until the driver signals completion
    let (lock, cvar) = &*result;
    let mut guard = lock.lock().unwrap();
    while guard.is_none() {
        guard = cvar.wait(guard).unwrap();
    }
    guard.take().unwrap()
}

// ---------------------------------------------------------------------------
// Driver thread — single "main" context that safely calls do_yield()
// ---------------------------------------------------------------------------

struct Waiter {
    vpid: u32,
    result: std::sync::Arc<(std::sync::Mutex<Option<i32>>, std::sync::Condvar)>,
}

static WAITERS: std::sync::Mutex<Vec<Waiter>> = std::sync::Mutex::new(Vec::new());
static DRIVER_STARTED: std::sync::Once = std::sync::Once::new();

fn start_driver_once() {
    DRIVER_STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("vproc-driver".into())
            .spawn(run_driver_loop)
            .expect("failed to spawn vproc driver thread");
    });
}

fn run_driver_loop() {
    loop {
        // Drive the scheduler
        {
            let _guard = EXECUTOR_MUTEX.lock().unwrap();
            let ptr = crate::executor::get_global_executor();
            if ptr.is_null() {
                drop(_guard);
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        }
        // Mutex must be released before yield
        crate::executor::do_yield();

        // Check waiters
        let completed: Vec<(u32, i32, std::sync::Arc<(std::sync::Mutex<Option<i32>>, std::sync::Condvar)>)> = {
            let mut waiters = WAITERS.lock().unwrap();
            let mut done = Vec::new();
            waiters.retain(|w| {
                if let Some(code) = crate::executor::get_exit_code(w.vpid) {
                    done.push((w.vpid, code, std::sync::Arc::clone(&w.result)));
                    false
                } else {
                    true
                }
            });
            done
        };

        // Signal completed waiters
        for (_vpid, code, result) in completed {
            let (lock, cvar) = &*result;
            *lock.lock().unwrap() = Some(code);
            cvar.notify_all();
        }

        // If no waiters, sleep briefly to avoid busy-loop
        if WAITERS.lock().unwrap().is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Get the per-coroutine working directory.
/// Returns 0 on success, -1 if no cwd set or vpid not found.
#[no_mangle]
pub extern "C" fn vproc_ffi_get_cwd(vpid: u32, buf: *mut c_char, size: usize) -> c_int {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return -1;
    }
    unsafe {
        match (*ptr).vprocs.get(&vpid) {
            Some(co) => match &co.cwd {
                Some(cwd) => {
                    let bytes = cwd.as_bytes();
                    if bytes.len() + 1 > size {
                        return -1;
                    }
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
                    *buf.add(bytes.len()) = 0;
                    0
                }
                None => -1,
            },
            None => -1,
        }
    }
}
