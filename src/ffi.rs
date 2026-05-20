//! C FFI interface for the vproc runtime.
//!
//! Exported by libvproc.so, called by the C preload layer (libvproc_preload.so).

use std::os::raw::{c_char, c_int, c_void};

/// Returns the current virtual process ID, or 0 if not in a coroutine.
/// No mutex: called from every syscall interceptor in coroutines (driver thread).
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
/// No mutex: read-only, stale data is acceptable.
#[no_mangle]
pub extern "C" fn vproc_ffi_vpid_exists(vpid: u32) -> c_int {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return 0;
    }
    if unsafe { (*ptr).vprocs.contains_key(&vpid) } { 1 } else { 0 }
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
/// No mutex: called from GOT-patched code inside coroutines (driver thread).
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
/// No mutex: called from GOT-patched code inside coroutines (driver thread).
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
///
/// No mutex: called from inside a coroutine (driver thread), already serialized.
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
//
// Thread safety: Java threads never touch the Executor directly.
// They submit spawn requests to SPAWN_QUEUE and register exit waiters
// in WAITERS. The driver thread is the sole owner of the Executor —
// it drains the queue, spawns coroutines, and signals completion.
// ---------------------------------------------------------------------------

/// A spawn request submitted by a Java thread, processed by the driver thread.
struct SpawnRequest {
    path: String,
    argv: Vec<String>,
    envp: Vec<String>,
    stdin_fd: c_int,
    stdout_fd: c_int,
    stderr_fd: c_int,
    /// Driver sets this to Some(vpid) on success or Some(0) on failure.
    result: std::sync::Arc<(std::sync::Mutex<Option<u32>>, std::sync::Condvar)>,
}

static SPAWN_QUEUE: std::sync::Mutex<Vec<SpawnRequest>> = std::sync::Mutex::new(Vec::new());

/// Create a virtual process: load binary in coroutine, map fd 0/1/2 to real fds.
///
/// Returns virtual PID (> 0) on success, 0 on error.
/// Thread-safe: submits spawn request to driver thread via queue.
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

    let result = std::sync::Arc::new((
        std::sync::Mutex::new(None::<u32>),
        std::sync::Condvar::new(),
    ));

    SPAWN_QUEUE.lock().unwrap().push(SpawnRequest {
        path: path_str,
        argv: argv_vec,
        envp: envp_vec,
        stdin_fd,
        stdout_fd,
        stderr_fd,
        result: std::sync::Arc::clone(&result),
    });

    start_driver_once();
    // Wake driver thread to process the new request
    DRIVER_WAKE.notify_one();

    // Block until driver processes our request
    let (lock, cvar) = &*result;
    let mut guard = lock.lock().unwrap();
    while guard.is_none() {
        guard = cvar.wait(guard).unwrap();
    }
    guard.take().unwrap()
}

/// Drive the scheduler until the given vpid exits.
/// Returns the exit code. Blocks the calling thread.
/// Thread-safe: registers a waiter, driver thread signals completion.
#[no_mangle]
pub extern "C" fn vproc_ffi_run_until_exit(vpid: u32) -> c_int {
    let result = std::sync::Arc::new((
        std::sync::Mutex::new(None::<i32>),
        std::sync::Condvar::new(),
    ));

    WAITERS.lock().unwrap().push(Waiter {
        vpid,
        result: std::sync::Arc::clone(&result),
    });

    start_driver_once();
    // Wake driver thread to check for completion
    DRIVER_WAKE.notify_one();

    // Block until the driver signals completion
    let (lock, cvar) = &*result;
    let mut guard = lock.lock().unwrap();
    while guard.is_none() {
        guard = cvar.wait(guard).unwrap();
    }
    guard.take().unwrap()
}

// ---------------------------------------------------------------------------
// Driver thread — sole owner of the Executor
// ---------------------------------------------------------------------------

struct Waiter {
    vpid: u32,
    result: std::sync::Arc<(std::sync::Mutex<Option<i32>>, std::sync::Condvar)>,
}

static WAITERS: std::sync::Mutex<Vec<Waiter>> = std::sync::Mutex::new(Vec::new());
static DRIVER_STARTED: std::sync::Once = std::sync::Once::new();

/// Condvar for waking the driver thread when new work arrives.
/// Both SPAWN_QUEUE pushes and WAITERS pushes signal this.
static DRIVER_WAKE: std::sync::Condvar = std::sync::Condvar::new();
static DRIVER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn start_driver_once() {
    DRIVER_STARTED.call_once(|| {
        std::thread::Builder::new()
            .name("vproc-driver".into())
            .spawn(run_driver_loop)
            .expect("failed to spawn vproc driver thread");
    });
}

/// Raw dup3 syscall — bypasses LD_PRELOAD interceptors.
/// aarch64 has no __NR_dup2; dup3(old, new, 0) is equivalent.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn raw_dup3(old_fd: c_int, new_fd: c_int) {
    if old_fd < 0 || old_fd == new_fd {
        return;
    }
    let ret: isize;
    std::arch::asm!(
        "mov x8, #24",
        "svc #0",
        lateout("x0") ret,
        in("x0") old_fd,
        in("x1") new_fd,
        in("x2") 0usize,
    );
    if ret < 0 {
        let msg = format!("vproc: raw_dup3({}, {}) failed\n", old_fd, new_fd);
        libc::syscall(64, 2, msg.as_ptr(), msg.len());
    }
}

/// Save driver thread's real fd 0/1/2, install the coroutine's real fds
/// (from vfd table), run one scheduler step, then restore original fds.
/// This ensures real fork children inherit the correct fds from the
/// currently-running coroutine's fd table.
fn run_driver_loop() {
    // Save driver thread's original fds on first iteration.
    let saved_fds: [c_int; 3] = unsafe {
        [
            libc::dup(0),
            libc::dup(1),
            libc::dup(2),
        ]
    };
    for &fd in &saved_fds {
        if fd < 0 {
            let msg = format!("vproc: failed to save driver fd (got {})\n", fd);
            unsafe { libc::syscall(64, 2, msg.as_ptr(), msg.len()); }
            std::process::abort();
        }
    }

    loop {
        // 1. Drain spawn queue — driver owns the Executor exclusively
        let requests: Vec<SpawnRequest> = SPAWN_QUEUE.lock().unwrap().drain(..).collect();
        for req in requests {
            let vpid = match crate::vexec::virtual_execve_via_entry(&req.path, req.argv, req.envp) {
                Ok(exec) => {
                    crate::vfd::create_table_with_fds(exec.vpid, req.stdin_fd, req.stdout_fd, req.stderr_fd);
                    exec.vpid
                }
                Err(e) => {
                    let msg = format!("vproc_ffi_create_process: {}\n", e);
                    unsafe { libc::syscall(64, 2, msg.as_ptr(), msg.len()); }
                    0
                }
            };
            let (lock, cvar) = &*req.result;
            *lock.lock().unwrap() = Some(vpid);
            cvar.notify_all();
        }

        // 2. Drive the scheduler — fd swap is encapsulated in Executor
        let ptr = crate::executor::get_global_executor();
        if !ptr.is_null() {
            unsafe {
                let ex = &mut *ptr;
                ex.step_from_driver_with_fd_swap(saved_fds);
            }
        }

        // 3. Check waiters — signal completed ones
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
        for (vpid, code, result) in completed {
            let (lock, cvar) = &*result;
            *lock.lock().unwrap() = Some(code);
            cvar.notify_all();
            crate::executor::remove_exit_code(vpid);
        }

        // 3b. Reap done coroutines
        let ptr = crate::executor::get_global_executor();
        if !ptr.is_null() {
            unsafe { (*ptr).reap_done_coroutines(); }
        }

        // 4. Block until new work arrives (spawn request or waiter registration)
        if WAITERS.lock().unwrap().is_empty() && SPAWN_QUEUE.lock().unwrap().is_empty() {
            let guard = DRIVER_LOCK.lock().unwrap();
            if WAITERS.lock().unwrap().is_empty() && SPAWN_QUEUE.lock().unwrap().is_empty() {
                let _guard = DRIVER_WAKE.wait(guard);
            }
        }
    }
}

/// Get the per-coroutine working directory.
/// Returns 0 on success, -1 if no cwd set or vpid not found.
/// No mutex: read-only, called from coroutine context (driver thread).
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
