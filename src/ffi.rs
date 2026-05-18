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
            unsafe { (**pipe_buf).read_from(dst) }
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
            unsafe { (**pipe_buf).write_to(src) }
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
            crate::vfd::Vfd::PipeWrite(buf) => unsafe {
                if (**buf).is_closed() {
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
        Some(t) => match t.close(fd as u32) {
            Ok(()) => 0,
            Err(_) => -1,
        },
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
    crate::executor::EXECUTOR.with(|e| unsafe {
        match (*e.get()).current {
            Some(pid) => pid,
            None => libc::getpid() as u32,
        }
    })
}

/// Get the parent virtual process ID. Returns real PPID if not in a coroutine.
#[no_mangle]
pub extern "C" fn vproc_ffi_getppid() -> u32 {
    crate::executor::EXECUTOR.with(|e| unsafe {
        let ex = &*e.get();
        match ex.current {
            Some(pid) => ex
                .vprocs
                .get(&pid)
                .map(|co| co.ppid)
                .unwrap_or(0),
            None => libc::getppid() as u32,
        }
    })
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
            eprintln!("vproc: virtual_execve: {}", e);
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
    // Phase 1: check if we are a fork child resuming after do_yield.
    // fork_child_pid was initialized to 0 by fork_from, so children
    // return 0 from this path. Parents return the actual child pid.
    let fork_result = crate::executor::EXECUTOR.with(|e| unsafe {
        let ex = &mut *e.get();
        let pid = ex.current.unwrap();
        let co = ex.vprocs.get(&pid).unwrap();

        if co.is_fork_child {
            // First time child runs: reset flag so subsequent calls work
            ex.vprocs.get_mut(&pid).unwrap().is_fork_child = false;
            return 0u32;
        }

        // Not a child yet — return sentinel to proceed with parent path
        u32::MAX
    });

    if fork_result != u32::MAX {
        return fork_result;
    }

    // Phase 2 (parent): spawn helper to copy our stack, then yield.
    crate::spawn(Box::new(move || {
        crate::executor::EXECUTOR.with(|e| unsafe {
            (&mut *e.get()).spawn_fork_child();
        });
    }));

    crate::executor::do_yield();

    // Phase 3: parent resumes — read child pid from our Coroutine.
    // Children also reach here but fork_child_pid is 0, so they return 0.
    crate::executor::EXECUTOR.with(|e| unsafe {
        let ex = &mut *e.get();
        let pid = ex.current.unwrap();
        ex.vprocs.get(&pid).unwrap().fork_child_pid
    })
}
