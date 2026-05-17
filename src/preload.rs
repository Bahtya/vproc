//! LD_PRELOAD interception layer for vproc.

use std::os::raw::{c_char, c_int, c_void};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn enabled() -> bool {
    static CHECKED: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(0);
    static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let v = CHECKED.load(std::sync::atomic::Ordering::Relaxed);
    if v == 0 {
        let e = std::env::var("VPROC").unwrap_or_default();
        let on = e == "1";
        ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
        CHECKED.store(1, std::sync::atomic::Ordering::Relaxed);
        on
    } else {
        ENABLED.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Resolve a real libc function via dlsym(RTLD_NEXT).
unsafe fn real(sym: &'static str) -> *mut c_void {
    static mut CACHE: [(*const u8, *mut c_void); 16] = [(std::ptr::null(), std::ptr::null_mut()); 16];
    static mut COUNT: usize = 0;

    for i in 0..COUNT {
        if CACHE[i].0 == sym.as_ptr() {
            return CACHE[i].1;
        }
    }

    let rtld_next = -1isize as *mut c_void;
    let ptr = libc::dlsym(rtld_next, sym.as_ptr() as *const c_char);
    if ptr.is_null() {
        eprintln!("vproc: cannot resolve {:?}", sym);
        libc::_exit(99);
    }
    if COUNT < 16 {
        CACHE[COUNT] = (sym.as_ptr(), ptr);
        COUNT += 1;
    }
    ptr
}

fn current_vpid() -> Option<crate::coroutine::VPid> {
    crate::executor::EXECUTOR.with(|e| unsafe { (*e.get()).current })
}

// ---------------------------------------------------------------------------
// exit() / _exit()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _exit(code: c_int) -> ! {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> ! = std::mem::transmute(real("_exit\0"));
            f(code);
        }
    }
    crate::executor::vproc_exit_with_code(code);
    unreachable!()
}

#[no_mangle]
pub extern "C" fn exit(code: c_int) -> ! {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> ! = std::mem::transmute(real("_exit\0"));
            f(code);
        }
    }
    crate::executor::vproc_exit_with_code(code);
    unreachable!()
}

// ---------------------------------------------------------------------------
// fork() — virtual fork returns ENOSYS (needs full stack copy)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn fork() -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("fork\0"));
            return f();
        }
    }
    unsafe { *libc::__errno() = 38 }; // ENOSYS
    -1
}

#[no_mangle]
pub extern "C" fn vfork() -> c_int {
    fork()
}

// ---------------------------------------------------------------------------
// waitpid() / wait4()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, *mut c_int, c_int) -> c_int =
                std::mem::transmute(real("waitpid\0"));
            return f(pid, status, options);
        }
    }

    let vpid = pid as u32;
    loop {
        if let Some(code) = crate::executor::get_exit_code(vpid) {
            if !status.is_null() {
                unsafe { *status = (code & 0xff) << 8 };
            }
            return pid;
        }
        let exists = crate::executor::EXECUTOR.with(|e| unsafe {
            (*e.get()).vprocs.contains_key(&vpid)
        });
        if !exists {
            return -1;
        }
        crate::executor::do_yield();
    }
}

#[no_mangle]
pub extern "C" fn wait4(
    pid: c_int,
    status: *mut c_int,
    options: c_int,
    _rusage: *mut c_void,
) -> c_int {
    waitpid(pid, status, options)
}

// ---------------------------------------------------------------------------
// execve()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int =
                std::mem::transmute(real("execve\0"));
            return f(path, argv, envp);
        }
    }

    let path_str = unsafe { std::ffi::CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    let argv_vec = unsafe { c_array_to_vec(argv) };
    let envp_vec = unsafe { c_array_to_vec(envp) };

    match crate::vexec::virtual_execve(&path_str, argv_vec, envp_vec) {
        Ok(_) => {
            crate::executor::vproc_exit_with_code(0);
            unreachable!()
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
        let s = std::ffi::CStr::from_ptr(*ptr).to_string_lossy().into_owned();
        vec.push(s);
        ptr = ptr.add(1);
    }
    vec
}

// ---------------------------------------------------------------------------
// pipe()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn pipe(fds: *mut c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(*mut c_int) -> c_int = std::mem::transmute(real("pipe\0"));
            return f(fds);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*mut c_int) -> c_int = std::mem::transmute(real("pipe\0"));
            return f(fds);
        }
    };
    let table = crate::vfd::get_or_create_table(vpid);
    let (read_fd, write_fd) = table.create_pipe();
    unsafe {
        *fds = read_fd as c_int;
        *fds.add(1) = write_fd as c_int;
    }
    0
}

// ---------------------------------------------------------------------------
// read() / write()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                std::mem::transmute(real("read\0"));
            return f(fd, buf, count);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                std::mem::transmute(real("read\0"));
            return f(fd, buf, count);
        }
    };

    // Check if this is a virtual fd
    let is_virtual = crate::vfd::get_table(vpid)
        .and_then(|t| t.get(fd as u32))
        .map(|vfd| !matches!(vfd, crate::vfd::Vfd::Real(_)))
        .unwrap_or(false);

    if !is_virtual {
        unsafe {
            let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                std::mem::transmute(real("read\0"));
            return f(fd, buf, count);
        }
    }

    // Virtual pipe read — yield if empty
    loop {
        if let Some(table) = crate::vfd::get_table(vpid) {
            if let Some(crate::vfd::Vfd::PipeRead(pipe_buf)) = table.get(fd as u32) {
                let dst = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, count) };
                let n = unsafe { (**pipe_buf).read_from(dst) };
                if n >= 0 {
                    return n;
                }
                crate::executor::do_yield();
            } else {
                unsafe { *libc::__errno() = libc::EBADF };
                return -1;
            }
        } else {
            unsafe { *libc::__errno() = libc::EBADF };
            return -1;
        }
    }
}

#[no_mangle]
pub extern "C" fn write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, *const c_void, usize) -> isize =
                std::mem::transmute(real("write\0"));
            return f(fd, buf, count);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, *const c_void, usize) -> isize =
                std::mem::transmute(real("write\0"));
            return f(fd, buf, count);
        }
    };

    let is_virtual = crate::vfd::get_table(vpid)
        .and_then(|t| t.get(fd as u32))
        .map(|vfd| !matches!(vfd, crate::vfd::Vfd::Real(_)))
        .unwrap_or(false);

    if !is_virtual {
        unsafe {
            let f: extern "C" fn(c_int, *const c_void, usize) -> isize =
                std::mem::transmute(real("write\0"));
            return f(fd, buf, count);
        }
    }

    // Virtual pipe write — yield if full
    loop {
        if let Some(table) = crate::vfd::get_table(vpid) {
            if let Some(crate::vfd::Vfd::PipeWrite(pipe_buf)) = table.get(fd as u32) {
                let src = unsafe { std::slice::from_raw_parts(buf as *const u8, count) };
                let n = unsafe { (**pipe_buf).write_to(src) };
                if n >= 0 {
                    return n;
                }
                if unsafe { (**pipe_buf).is_closed() } {
                    unsafe { *libc::__errno() = libc::EPIPE };
                    return -1;
                }
                crate::executor::do_yield();
            } else {
                unsafe { *libc::__errno() = libc::EBADF };
                return -1;
            }
        } else {
            unsafe { *libc::__errno() = libc::EBADF };
            return -1;
        }
    }
}

// ---------------------------------------------------------------------------
// close() / dup() / dup2()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn close(fd: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
            return f(fd);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
            return f(fd);
        }
    };
    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
            return f(fd);
        }
    };
    match table.get(fd as u32) {
        Some(crate::vfd::Vfd::Real(_)) => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
            f(fd)
        },
        Some(_) => match table.close(fd as u32) {
            Ok(()) => 0,
            Err(_) => {
                unsafe { *libc::__errno() = libc::EBADF };
                -1
            }
        },
        None => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
            f(fd)
        },
    }
}

#[no_mangle]
pub extern "C" fn dup(old_fd: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("dup\0"));
            return f(old_fd);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("dup\0"));
            return f(old_fd);
        }
    };
    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("dup\0"));
            return f(old_fd);
        }
    };
    match table.dup(old_fd as u32) {
        Ok(new_fd) => new_fd as c_int,
        Err(_) => {
            unsafe { *libc::__errno() = libc::EBADF };
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn dup2(old_fd: c_int, new_fd: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("dup2\0"));
            return f(old_fd, new_fd);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("dup2\0"));
            return f(old_fd, new_fd);
        }
    };
    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => unsafe {
            let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("dup2\0"));
            return f(old_fd, new_fd);
        }
    };
    match table.dup2(old_fd as u32, new_fd as u32) {
        Ok(fd) => fd as c_int,
        Err(_) => {
            unsafe { *libc::__errno() = libc::EBADF };
            -1
        }
    }
}
