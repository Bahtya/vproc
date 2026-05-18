//! LD_PRELOAD interception layer for vproc.

use std::os::raw::{c_char, c_int, c_void};

// ---------------------------------------------------------------------------
// SIGSEGV handler — install early for crash diagnosis
// ---------------------------------------------------------------------------

/// Install a SIGSEGV handler that prints fault address and fp-based backtrace.
pub fn install_crash_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = crash_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }
}

extern "C" fn crash_handler(
    sig: c_int,
    info: *mut libc::siginfo_t,
    _ctx: *mut c_void,
) {
    let fault_addr = unsafe { (*info).si_addr() as usize };
    let mut buf = [0u8; 256];
    let msg = unsafe {
        let n = libc::snprintf(
            buf.as_mut_ptr() as *mut c_char,
            256,
            b"\n[SIGSEGV] signal=%d fault_addr=%p\n\0".as_ptr() as *const c_char,
            sig,
            fault_addr,
        );
        core::str::from_utf8_unchecked(&buf[..n as usize])
    };
    unsafe { libc::syscall(64, 2, msg.as_ptr() as *const _, msg.len()); }

    // Walk fp chain for backtrace
    let mut fp: usize;
    unsafe { std::arch::asm!("mov {}, x29", out(reg) fp); }
    for i in 0..16 {
        if fp == 0 { break; }
        let lr = unsafe { std::ptr::read_unaligned((fp + 8) as *const usize) };
        let msg = unsafe {
            let n = libc::snprintf(
                buf.as_mut_ptr() as *mut c_char,
                256,
                b"  #%d fp=%p lr=%p\n\0".as_ptr() as *const c_char,
                i,
                fp,
                lr,
            );
            core::str::from_utf8_unchecked(&buf[..n as usize])
        };
        unsafe { libc::syscall(64, 2, msg.as_ptr() as *const _, msg.len()); }
        fp = unsafe { std::ptr::read_unaligned(fp as *const usize) };
    }

    // Re-raise to get core dump / default behavior
    unsafe {
        libc::_exit(128 + sig);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Global enabled flag. -1 = unchecked, 0 = disabled, 1 = enabled.
/// Must survive TLS reinitialization by __libc_init in dlopen'd binaries.
static VPROC_ENABLED: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

fn enabled() -> bool {
    let v = VPROC_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    if v == 1 {
        return true;
    }
    // Check env every time until we see "1" — the env var may be set
    // after program start (e.g. std::env::set_var in main).
    let val = unsafe { libc::getenv(b"VPROC\0".as_ptr() as *const c_char) };
    let on = !val.is_null() && unsafe { *val == b'1' as _ };
    if on {
        VPROC_ENABLED.store(1, std::sync::atomic::Ordering::Relaxed);
        install_crash_handler();
    }
    on
}

/// Resolve a real libc function via dlsym(RTLD_NEXT).
unsafe fn real(sym: &'static str) -> *mut c_void {
    static mut CACHE: [(*const u8, *mut c_void); 32] = [(std::ptr::null(), std::ptr::null_mut()); 32];
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
    if COUNT < 32 {
        CACHE[COUNT] = (sym.as_ptr(), ptr);
        COUNT += 1;
    }
    ptr
}

fn current_vpid() -> Option<crate::coroutine::VPid> {
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        return None;
    }
    unsafe { (*ptr).current }
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
    if current_vpid().is_some() {
        crate::executor::vproc_exit_with_code(code);
    }
    unsafe {
        std::arch::asm!(
            "mov x8, #94",
            "svc #0",
            in("x0") code,
            options(noreturn)
        );
    }
}

#[no_mangle]
pub extern "C" fn exit(code: c_int) -> ! {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> ! = std::mem::transmute(real("_exit\0"));
            f(code);
        }
    }
    let vpid = current_vpid();
    if vpid.is_some() {
        crate::executor::vproc_exit_with_code(code);
    }
    unsafe {
        std::arch::asm!(
            "mov x8, #94",
            "svc #0",
            in("x0") code,
            options(noreturn)
        );
    }
}

// ---------------------------------------------------------------------------
// getpid() / getppid()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn getpid() -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("getpid\0"));
            return f();
        }
    }
    match current_vpid() {
        Some(p) => p as c_int,
        None => unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("getpid\0"));
            f()
        },
    }
}

#[no_mangle]
pub extern "C" fn getppid() -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("getppid\0"));
            return f();
        }
    }
    let ptr = crate::executor::get_global_executor();
    if ptr.is_null() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("getppid\0"));
            return f();
        }
    }
    unsafe {
        let ex = &*ptr;
        match (*ex).current {
            Some(pid) => ex
                .vprocs
                .get(&pid)
                .map(|co| co.ppid as c_int)
                .unwrap_or(0),
            None => {
                let f: extern "C" fn() -> c_int = std::mem::transmute(real("getppid\0"));
                f()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// fork() / vfork() — virtual fork
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn fork() -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("fork\0"));
            return f();
        }
    }
    let r = crate::ffi::vproc_ffi_fork() as c_int;
    r
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
        let exists = {
            let ptr = crate::executor::get_global_executor();
            !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&vpid) }
        };
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
    EXECVE_CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path_str = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy();

    if !enabled() {
        // Use raw syscall to avoid recursion with hook_libc_execve
        unsafe {
            let ret: isize;
            std::arch::asm!(
                "mov x8, #221",  // __NR_execve on aarch64
                "svc #0",
                lateout("x0") ret,
                in("x1") argv,
                in("x2") envp,
                in("x0") path,
            );
            if ret < 0 {
                *libc::__errno() = (-ret) as c_int;
                return -1;
            }
            return ret as c_int;
        }
    }

    let argv_vec = unsafe { crate::c_array_to_vec(argv) };
    let envp_vec = unsafe { crate::c_array_to_vec(envp) };

    match crate::vexec::virtual_execve(&*path_str, argv_vec, envp_vec) {
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

/// Get number of times execve interceptor was called (for testing).
pub fn get_execve_call_count() -> usize {
    EXECVE_CALL_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

static EXECVE_CALL_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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

// ---------------------------------------------------------------------------
// kill()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn kill(pid: c_int, sig: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("kill\0"));
            return f(pid, sig);
        }
    }
    // For positive pids, check if it's a virtual process
    if pid > 0 {
        let ptr = crate::executor::get_global_executor();
        let exists = if ptr.is_null() {
            false
        } else {
            unsafe { (*ptr).vprocs.contains_key(&(pid as u32)) }
        };
        if exists {
            // Virtual process — signal delivery not yet implemented, return success
            return 0;
        }
    }
    // Real process or process group (negative pid / pid=0) — pass through
    unsafe {
        let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("kill\0"));
        f(pid, sig)
    }
}

// ---------------------------------------------------------------------------
// getpgid() / setpgid()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn getpgid(pid: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("getpgid\0"));
            return f(pid);
        }
    }
    // For virtual pids, return a fake pgid (just the pid itself)
    if pid > 0 {
        let ptr = crate::executor::get_global_executor();
        let exists = !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&(pid as u32)) };
        if exists {
            return pid;
        }
    }
    // pid == 0 means "current process"
    if pid == 0 {
        if let Some(vpid) = current_vpid() {
            return vpid as c_int;
        }
    }
    // Real process — pass through
    unsafe {
        let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("getpgid\0"));
        f(pid)
    }
}

#[no_mangle]
pub extern "C" fn setpgid(pid: c_int, pgid: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("setpgid\0"));
            return f(pid, pgid);
        }
    }
    // For virtual pids, stub: return success
    if pid > 0 {
        let ptr = crate::executor::get_global_executor();
        let exists = !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&(pid as u32)) };
        if exists {
            return 0;
        }
    }
    if pid == 0 {
        if current_vpid().is_some() {
            return 0;
        }
    }
    // Real process — pass through
    unsafe {
        let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("setpgid\0"));
        f(pid, pgid)
    }
}

// ---------------------------------------------------------------------------
// raise()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn raise(sig: c_int) -> c_int {
    // raise() sends a signal to the current (real) process/thread.
    // In vproc the "current process" is the real OS process, so always
    // pass through.
    unsafe {
        let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("raise\0"));
        f(sig)
    }
}
