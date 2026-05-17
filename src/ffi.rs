//! C FFI interface for the vproc runtime.
//!
//! Exported by libvproc.so, called by the C preload layer (libvproc_preload.so).

use std::os::raw::{c_char, c_int, c_void};

/// Returns the current virtual process ID, or 0 if not in a coroutine.
#[no_mangle]
pub extern "C" fn vproc_ffi_current_vpid() -> u32 {
    crate::executor::EXECUTOR.with(|e| unsafe { (*e.get()).current.unwrap_or(0) })
}

/// Exit the current virtual process. Does not return.
#[no_mangle]
pub extern "C" fn vproc_ffi_exit(code: c_int) {
    crate::executor::vproc_exit_with_code(code);
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
    let argv_vec = unsafe { c_array_to_vec(argv) };
    let envp_vec = unsafe { c_array_to_vec(envp) };

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

unsafe fn c_array_to_vec(arr: *const *const c_char) -> Vec<String> {
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
