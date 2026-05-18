//! LD_PRELOAD interception layer for vproc.

use std::cell::UnsafeCell;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// SIGSEGV handler — install early for crash diagnosis
// ---------------------------------------------------------------------------

static CRASH_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install a SIGSEGV handler that prints fault address and fp-based backtrace.
/// Safe to call multiple times — only installs once.
pub fn install_crash_handler() {
    if CRASH_HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
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
    // Keep checking env until we see "1" — VPROC may be set after
    // program start (e.g. std::env::set_var in main), or during
    // early init before the env var is visible.
    let val = unsafe { libc::getenv(b"VPROC\0".as_ptr() as *const c_char) };
    let on = !val.is_null() && unsafe { *val == b'1' as _ };
    if on {
        VPROC_ENABLED.store(1, std::sync::atomic::Ordering::Relaxed);
        install_crash_handler();
    }
    on
}

/// Resolve a real libc function via dlsym(RTLD_NEXT).
/// Uses a dlsym cache with manual synchronization (cooperative scheduling
/// guarantees single-threaded access; AtomicUsize for count ensures safe init).
struct DlsymCache(UnsafeCell<[(*const u8, *mut c_void); 32]>);
unsafe impl Sync for DlsymCache {}

unsafe fn real(sym: &'static str) -> *mut c_void {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: DlsymCache = DlsymCache(UnsafeCell::new([(std::ptr::null(), std::ptr::null_mut()); 32]));
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    let count = COUNT.load(Ordering::Acquire);
    let cache = &*CACHE.0.get();
    for i in 0..count {
        if cache[i].0 == sym.as_ptr() {
            return cache[i].1;
        }
    }

    let rtld_next = -1isize as *mut c_void;
    let ptr = libc::dlsym(rtld_next, sym.as_ptr() as *const c_char);
    if ptr.is_null() {
        let msg = format!("vproc: cannot resolve {:?}\n", sym);
        libc::syscall(64, 2, msg.as_ptr() as *const _, msg.len());
        libc::_exit(99);
    }
    let idx = COUNT.fetch_add(1, Ordering::AcqRel);
    if idx < 32 {
        (*CACHE.0.get())[idx] = (sym.as_ptr(), ptr);
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
// Real fork child flag — set in child after real fork(), checked by all
// interceptors to pass through to real syscalls.
// ---------------------------------------------------------------------------

static REAL_FORK_CHILD: AtomicBool = AtomicBool::new(false);

fn is_real_fork_child() -> bool {
    REAL_FORK_CHILD.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// exit() / _exit()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _exit(code: c_int) -> ! {
    if !enabled() || is_real_fork_child() {
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
    if !enabled() || is_real_fork_child() {
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
        match ex.current {
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
// fork() / vfork() — use real fork for memory isolation
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn fork() -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn() -> c_int = std::mem::transmute(real("fork\0"));
            return f();
        }
    }
    // Use real() (dlsym RTLD_NEXT) to get the true libc fork,
    // not libc::fork() which would resolve to our own symbol.
    let pid = unsafe {
        let f: extern "C" fn() -> c_int = std::mem::transmute(real("fork\0"));
        f()
    };
    if pid == 0 {
        REAL_FORK_CHILD.store(true, Ordering::SeqCst);
    }
    pid
}

#[no_mangle]
pub extern "C" fn vfork() -> c_int {
    fork()
}

// ---------------------------------------------------------------------------
// waitpid() / wait4()
// ---------------------------------------------------------------------------

/// Raw wait4 syscall — bypasses libc entirely to avoid waitpid→wait4 recursion.
#[cfg(target_arch = "aarch64")]
fn raw_wait4(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    let ret = unsafe {
        libc::syscall(260, pid, status, options, 0usize) // __NR_wait4
    };
    if ret < 0 {
        unsafe { *libc::__errno() = (-ret) as c_int; }
        return -1;
    }
    ret as c_int
}

#[no_mangle]
pub extern "C" fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    if !enabled() || is_real_fork_child() {
        return raw_wait4(pid, status, options);
    }
    // Check if this is a virtual process (exists in vproc executor)
    let vpid = pid as u32;
    let is_virtual = {
        let ptr = crate::executor::get_global_executor();
        !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&vpid) }
    };
    if !is_virtual {
        // Real child process — use raw wait4 syscall to avoid recursion:
        // libc waitpid() internally calls wait4(), which we also intercept,
        // causing waitpid → libc waitpid → libc wait4 → our wait4 → our waitpid.
        return raw_wait4(pid, status, options);
    }
    // Virtual process — spin/yield until done
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
    // Use raw syscall directly — delegating to waitpid() would re-enter
    // our interceptor and potentially trigger the waitpid→wait4 recursion.
    raw_wait4(pid, status, options)
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

    if !enabled() || is_real_fork_child() {
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
            let msg = format!("vproc: virtual_execve: {}\n", e);
            unsafe { libc::syscall(64, 2, msg.as_ptr(), msg.len()); }
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
    // Use real pipe() so that real fork() children inherit working pipe fds.
    // Virtual pipes can't cross real fork() boundaries.
    unsafe {
        let f: extern "C" fn(*mut c_int) -> c_int = std::mem::transmute(real("pipe\0"));
        f(fds)
    }
}

// ---------------------------------------------------------------------------
// read() / write()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    if !enabled() || is_real_fork_child() {
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
                let n = pipe_buf.read_from(dst);
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
    if !enabled() || is_real_fork_child() {
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
                let n = pipe_buf.write_to(src);
                if n >= 0 {
                    return n;
                }
                if pipe_buf.is_closed() {
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
    if !enabled() || is_real_fork_child() {
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
    if !enabled() || is_real_fork_child() {
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
    if !enabled() || is_real_fork_child() {
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
