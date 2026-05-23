//! LD_PRELOAD interception layer for vproc.

use std::cell::UnsafeCell;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Library constructor — auto-set VPROC=1 on load
// ---------------------------------------------------------------------------
// Ensures integrators don't need to call setenv("VPROC","1") manually.
// Runs before any other library code via .init_array (Android/Linux).

mod init {
    use super::*;

    extern "C" fn vproc_auto_enable() {
        unsafe {
            libc::setenv(
                b"VPROC\0".as_ptr() as *const c_char,
                b"1\0".as_ptr() as *const c_char,
                1,
            );
        }
    }

    #[link_section = ".init_array"]
    #[used]
    static VPROC_INIT: extern "C" fn() = vproc_auto_enable;
}

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
        libc::sigaction(libc::SIGILL, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGABRT, &sa, std::ptr::null_mut());
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
    unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }

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
        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
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
struct DlsymCache(UnsafeCell<[(*const u8, *mut c_void); 64]>);
unsafe impl Sync for DlsymCache {}

unsafe fn real(sym: &'static str) -> *mut c_void {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: DlsymCache = DlsymCache(UnsafeCell::new([(std::ptr::null(), std::ptr::null_mut()); 64]));
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
        libc::write(2, msg.as_ptr() as *const _, msg.len());
        libc::_exit(99);
    }
    let idx = COUNT.fetch_add(1, Ordering::AcqRel);
    if idx < 64 {
        (*CACHE.0.get())[idx] = (sym.as_ptr(), ptr);
    }
    ptr
}

fn current_vpid() -> Option<crate::coroutine::VPid> {
    let ptr = crate::executor::get_current_executor();
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

/// Clear the fdsan ownership tag on an fd.
/// Prevents fdsan abort when closing fds owned by unique_fd objects in other
/// code (ART runtime, JNI bridge, etc.) — unavoidable when bash inherits all
/// process fds in our dlopen-based virtual exec model.
unsafe fn fdsan_clear_tag(fd: c_int) {
    use std::sync::OnceLock;
    type GetTagFn = unsafe extern "C" fn(c_int) -> u64;
    type ExchangeTagFn = unsafe extern "C" fn(c_int, u64, u64);
    static FUNCS: OnceLock<(Option<GetTagFn>, Option<ExchangeTagFn>)> = OnceLock::new();
    let (get_tag, exchange_tag) = FUNCS.get_or_init(|| {
        let gt = unsafe {
            libc::dlsym(libc::RTLD_DEFAULT, b"android_fdsan_get_fd_tag\0".as_ptr() as *const _)
        };
        let et = unsafe {
            libc::dlsym(libc::RTLD_DEFAULT, b"android_fdsan_exchange_owner_tag\0".as_ptr() as *const _)
        };
        (
            if gt.is_null() { None } else { Some(std::mem::transmute(gt)) },
            if et.is_null() { None } else { Some(std::mem::transmute(et)) },
        )
    });
    if let (Some(gt), Some(et)) = (get_tag, exchange_tag) {
        let tag = unsafe { gt(fd) };
        if tag != 0 {
            unsafe { et(fd, tag, 0); }
        }
    }
}

/// Call the real libc close() bypassing our interceptor.
pub unsafe fn real_close(fd: c_int) -> c_int {
    fdsan_clear_tag(fd);
    let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("close\0"));
    f(fd)
}

// ---------------------------------------------------------------------------
// Path resolution for per-coroutine cwd
// ---------------------------------------------------------------------------

fn resolve_path(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    let mut parts: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => { parts.pop(); }
            _ => parts.push(component),
        }
    }
    if parts.is_empty() { "/".to_string() } else { format!("/{}", parts.join("/")) }
}

/// Get the current coroutine's cwd, or None if using process cwd.
fn get_cwd() -> Option<String> {
    let vpid = current_vpid()?;
    let ptr = crate::executor::get_current_executor();
    if ptr.is_null() { return None; }
    unsafe { (*ptr).vprocs.get(&vpid).and_then(|co| co.cwd.clone()) }
}

/// Get the process's real cwd via real libc getcwd.
fn process_cwd() -> String {
    let mut buf = [0u8; 4096];
    unsafe {
        let f: extern "C" fn(*mut c_char, usize) -> *mut c_char =
            std::mem::transmute(real("getcwd\0"));
        let ptr = f(buf.as_mut_ptr() as *mut c_char, buf.len());
        if !ptr.is_null() {
            let len = libc::strlen(buf.as_ptr() as *const c_char);
            std::str::from_utf8(&buf[..len]).unwrap_or("/").to_string()
        } else {
            "/".to_string()
        }
    }
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
    // Raw exit_group syscall — bypass LD_PRELOAD, never returns.
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
    // Raw exit_group syscall — bypass LD_PRELOAD, never returns.
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
    let ptr = crate::executor::get_current_executor();
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
    // NOTE: do NOT use raw clone syscall here — bionic's fork() runs
    // pthread_atfork handlers that reset TLS, robust mutex lists, and
    // per-process bookkeeping. Raw clone skips these, leaving the child
    // in an inconsistent state.
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
        let ptr = crate::executor::get_current_executor();
        !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&vpid) }
    };
    if !is_virtual {
        // Real child process — poll with WNOHANG + yield to avoid blocking
        // the driver thread. Direct raw_wait4 with options=0 would deadlock
        // the scheduler if the real child hasn't exited yet.
        if options & libc::WNOHANG != 0 {
            return raw_wait4(pid, status, options);
        }
        loop {
            let ret = raw_wait4(pid, status, libc::WNOHANG);
            if ret > 0 { return ret; }
            if ret < 0 { return -1; }
            crate::executor::do_yield();
        }
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
            let ptr = crate::executor::get_current_executor();
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
        Err(_) => {
            // virtual_execve failed (e.g. binary format unsupported).
            // Fall back to real fork + exec + waitpid so that the caller
            // (e.g. bash last-command exec optimization) still works.
            let c_path = std::ffi::CString::new(path_str.into_owned())
                .unwrap_or_default();
            let pid = unsafe {
                let f: extern "C" fn() -> c_int = std::mem::transmute(real("fork\0"));
                f()
            };
            if pid < 0 {
                unsafe { *libc::__errno() = libc::ENOEXEC };
                return -1;
            }
            if pid == 0 {
                // Child: do real execve
                REAL_FORK_CHILD.store(true, Ordering::SeqCst);
                unsafe {
                    let ret: isize;
                    std::arch::asm!(
                        "mov x8, #221",
                        "svc #0",
                        lateout("x0") ret,
                        in("x1") argv,
                        in("x2") envp,
                        in("x0") c_path.as_ptr(),
                    );
                    // exec failed
                    let code: c_int = if ret < 0 { 126 } else { 0 };
                    std::arch::asm!(
                        "mov x8, #94",
                        "svc #0",
                        in("x0") code,
                        options(noreturn),
                    );
                }
            }
            // Parent: wait for child with polling (raw_wait4 with 0 would block
            // the driver thread). Use WNOHANG + yield like the waitpid interceptor.
            let mut status: c_int = 0;
            loop {
                let ret = raw_wait4(pid, &mut status, libc::WNOHANG);
                if ret > 0 { break; }
                if ret < 0 { status = 0; break; }
                crate::executor::do_yield();
            }
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                1
            };
            crate::executor::vproc_exit_with_code(code);
            unreachable!()
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

#[no_mangle]
pub extern "C" fn pipe2(fds: *mut c_int, flags: c_int) -> c_int {
    if !enabled() {
        unsafe {
            let f: extern "C" fn(*mut c_int, c_int) -> c_int = std::mem::transmute(real("pipe2\0"));
            return f(fds, flags);
        }
    }
    // Same as pipe() — use real pipe2 so real fork children inherit working fds.
    unsafe {
        let f: extern "C" fn(*mut c_int, c_int) -> c_int = std::mem::transmute(real("pipe2\0"));
        f(fds, flags)
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
    if fd < 0 {
        unsafe { *libc::__errno() = libc::EBADF };
        return -1;
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                std::mem::transmute(real("read\0"));
            return f(fd, buf, count);
        }
    };

    // Check if this is a virtual pipe fd
    let is_virtual = crate::vfd::get_table(vpid)
        .and_then(|t| t.get(fd as u32))
        .map(|vfd| matches!(vfd, crate::vfd::Vfd::PipeRead(_) | crate::vfd::Vfd::PipeWrite(_)))
        .unwrap_or(false);

    if !is_virtual {
        let real_fd = crate::vfd::get_table(vpid)
            .and_then(|t| t.get(fd as u32))
            .map(|vfd| match vfd {
                crate::vfd::Vfd::Real(r) => *r,
                crate::vfd::Vfd::File(f) => f.real_fd,
                _ => fd,
            })
            .unwrap_or(fd);
        // poll-before-read: check readiness with timeout=0 to avoid blocking the driver thread.
        loop {
            let mut pfd = libc::pollfd { fd: real_fd, events: libc::POLLIN, revents: 0 };
            let ready = unsafe {
                let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                    std::mem::transmute(real("poll\0"));
                f(&mut pfd, 1, 0)
            };
            if ready > 0 {
                if pfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    unsafe {
                        let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                            std::mem::transmute(real("read\0"));
                        return f(real_fd, buf, count);
                    }
                }
                // POLLERR/POLLNVAL — fall through to read to get proper errno
                unsafe {
                    let f: extern "C" fn(c_int, *mut c_void, usize) -> isize =
                        std::mem::transmute(real("read\0"));
                    return f(real_fd, buf, count);
                }
            }
            if ready < 0 { return -1; }
            // Not ready — yield waiting for I/O (driver thread will batch poll)
            crate::executor::yield_for_io(vec![(real_fd, libc::POLLIN)]);
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
    if fd < 0 {
        unsafe { *libc::__errno() = libc::EBADF };
        return -1;
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
        .map(|vfd| matches!(vfd, crate::vfd::Vfd::PipeRead(_) | crate::vfd::Vfd::PipeWrite(_)))
        .unwrap_or(false);

    if !is_virtual {
        let real_fd = crate::vfd::get_table(vpid)
            .and_then(|t| t.get(fd as u32))
            .map(|vfd| match vfd {
                crate::vfd::Vfd::Real(r) => *r,
                crate::vfd::Vfd::File(f) => f.real_fd,
                _ => fd,
            })
            .unwrap_or(fd);
        // poll-before-write: check readiness with timeout=0.
        loop {
            let mut pfd = libc::pollfd { fd: real_fd, events: libc::POLLOUT, revents: 0 };
            let ready = unsafe {
                let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                    std::mem::transmute(real("poll\0"));
                f(&mut pfd, 1, 0)
            };
            if ready > 0 {
                if pfd.revents & libc::POLLOUT != 0 {
                    unsafe {
                        let f: extern "C" fn(c_int, *const c_void, usize) -> isize =
                            std::mem::transmute(real("write\0"));
                        return f(real_fd, buf, count);
                    }
                }
                // POLLERR/POLLHUP — fall through to write to get proper errno
                unsafe {
                    let f: extern "C" fn(c_int, *const c_void, usize) -> isize =
                        std::mem::transmute(real("write\0"));
                    return f(real_fd, buf, count);
                }
            }
            if ready < 0 { return -1; }
            // Not ready — yield waiting for I/O
            crate::executor::yield_for_io(vec![(real_fd, libc::POLLOUT)]);
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
                    crate::executor::vproc_exit_with_code(128 + libc::SIGPIPE as i32);
                    unreachable!()
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
// poll() — cooperative I/O multiplexing
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn poll(fds: *mut libc::pollfd, nfds: libc::nfds_t, timeout: c_int) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                std::mem::transmute(real("poll\0"));
            return f(fds, nfds, timeout);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                std::mem::transmute(real("poll\0"));
            return f(fds, nfds, timeout);
        }
    };

    if fds.is_null() || nfds == 0 {
        unsafe {
            let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                std::mem::transmute(real("poll\0"));
            return f(fds, nfds, timeout);
        }
    }

    loop {
        let mut count = 0i32;
        let mut wait_fds: Vec<(c_int, i16)> = Vec::new();

        for i in 0..nfds as usize {
            let pfd = unsafe { &mut *fds.add(i) };
            pfd.revents = 0;

            let vfd = crate::vfd::get_table(vpid).and_then(|t| t.get(pfd.fd as u32));
            match vfd {
                Some(crate::vfd::Vfd::PipeRead(pipe)) => {
                    if pfd.events & libc::POLLIN != 0 {
                        if !pipe.is_empty() || pipe.is_closed() {
                            pfd.revents |= libc::POLLIN;
                            if pipe.is_closed() { pfd.revents |= libc::POLLHUP; }
                        }
                    }
                }
                Some(crate::vfd::Vfd::PipeWrite(pipe)) => {
                    if pfd.events & libc::POLLOUT != 0 {
                        if !pipe.is_full() {
                            pfd.revents |= libc::POLLOUT;
                        }
                        if pipe.is_closed() {
                            pfd.revents |= libc::POLLERR;
                        }
                    }
                }
                Some(crate::vfd::Vfd::Real(r)) => {
                    let real_fd = *r;
                    let mut check = libc::pollfd { fd: real_fd, events: pfd.events, revents: 0 };
                    let ready = unsafe {
                        let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                            std::mem::transmute(real("poll\0"));
                        f(&mut check, 1, 0)
                    };
                    if ready > 0 {
                        pfd.revents = check.revents;
                    } else {
                        wait_fds.push((real_fd, pfd.events));
                    }
                }
                Some(crate::vfd::Vfd::File(file_ref)) => {
                    let real_fd = file_ref.real_fd;
                    let mut check = libc::pollfd { fd: real_fd, events: pfd.events, revents: 0 };
                    let ready = unsafe {
                        let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                            std::mem::transmute(real("poll\0"));
                        f(&mut check, 1, 0)
                    };
                    if ready > 0 {
                        pfd.revents = check.revents;
                    } else {
                        wait_fds.push((real_fd, pfd.events));
                    }
                }
                None => {
                    // Unknown fd — real poll with timeout=0
                    let mut check = libc::pollfd { fd: pfd.fd, events: pfd.events, revents: 0 };
                    let ready = unsafe {
                        let f: extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int =
                            std::mem::transmute(real("poll\0"));
                        f(&mut check, 1, 0)
                    };
                    if ready > 0 {
                        pfd.revents = check.revents;
                    } else {
                        wait_fds.push((pfd.fd, pfd.events));
                    }
                }
            }

            if pfd.revents != 0 {
                count += 1;
            }
        }

        if count > 0 {
            return count;
        }
        if timeout == 0 {
            return 0;
        }

        // Nothing ready — yield waiting for I/O
        crate::executor::yield_for_io(wait_fds);
    }
}

// ---------------------------------------------------------------------------
// select() — convert to poll internally
// ---------------------------------------------------------------------------

const FD_SETSIZE: c_int = 1024;

unsafe fn fd_isset(fd: c_int, set: *const libc::fd_set) -> bool {
    let mask = 1u32 << (fd % 32);
    let idx = (fd / 32) as usize;
    let arr = set as *const [u32; 32];
    (*arr)[idx] & mask != 0
}

unsafe fn fd_set(fd: c_int, set: *mut libc::fd_set) {
    let mask = 1u32 << (fd % 32);
    let idx = (fd / 32) as usize;
    let arr = set as *mut [u32; 32];
    (*arr)[idx] |= mask;
}

#[no_mangle]
pub extern "C" fn select(
    nfds: c_int,
    readfds: *mut libc::fd_set,
    writefds: *mut libc::fd_set,
    exceptfds: *mut libc::fd_set,
    timeout: *mut libc::timeval,
) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(c_int, *mut libc::fd_set, *mut libc::fd_set, *mut libc::fd_set, *mut libc::timeval) -> c_int =
                std::mem::transmute(real("select\0"));
            return f(nfds, readfds, writefds, exceptfds, timeout);
        }
    }
    if current_vpid().is_none() {
        unsafe {
            let f: extern "C" fn(c_int, *mut libc::fd_set, *mut libc::fd_set, *mut libc::fd_set, *mut libc::timeval) -> c_int =
                std::mem::transmute(real("select\0"));
            return f(nfds, readfds, writefds, exceptfds, timeout);
        }
    }

    // Convert timeout to poll-style milliseconds
    let timeout_ms: c_int = if timeout.is_null() {
        -1 // block indefinitely
    } else {
        unsafe {
            let tv = &*timeout;
            ((tv.tv_sec as i64 * 1000) + (tv.tv_usec as i64 / 1000)) as c_int
        }
    };

    // Build pollfd array from fd_sets
    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    let max_fd = nfds.min(FD_SETSIZE);

    for fd in 0..max_fd {
        let mut events: i16 = 0;
        if !readfds.is_null() && unsafe { fd_isset(fd, readfds) } {
            events |= libc::POLLIN;
        }
        if !writefds.is_null() && unsafe { fd_isset(fd, writefds) } {
            events |= libc::POLLOUT;
        }
        if !exceptfds.is_null() && unsafe { fd_isset(fd, exceptfds) } {
            events |= libc::POLLPRI;
        }
        if events != 0 {
            pollfds.push(libc::pollfd { fd, events, revents: 0 });
        }
    }

    let ret = poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout_ms);

    // Clear fd_sets and set bits from results
    if !readfds.is_null() { unsafe { libc::memset(readfds as *mut _, 0, std::mem::size_of::<libc::fd_set>()); } }
    if !writefds.is_null() { unsafe { libc::memset(writefds as *mut _, 0, std::mem::size_of::<libc::fd_set>()); } }
    if !exceptfds.is_null() { unsafe { libc::memset(exceptfds as *mut _, 0, std::mem::size_of::<libc::fd_set>()); } }

    if ret > 0 {
        for pfd in &pollfds {
            if pfd.revents & libc::POLLIN != 0 && !readfds.is_null() {
                unsafe { fd_set(pfd.fd, readfds); }
            }
            if pfd.revents & libc::POLLOUT != 0 && !writefds.is_null() {
                unsafe { fd_set(pfd.fd, writefds); }
            }
            if pfd.revents & libc::POLLPRI != 0 && !exceptfds.is_null() {
                unsafe { fd_set(pfd.fd, exceptfds); }
            }
        }
    }

    ret
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
    if fd < 0 {
        unsafe { *libc::__errno() = libc::EBADF };
        return -1;
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
        Some(crate::vfd::Vfd::Real(_real_fd)) => {
            // Vfd::Real is a borrowed fd (e.g. caller's pipe fd passed via
            // vproc_ffi_create_process). Don't close the real fd — the caller
            // owns it. Just remove the vfd entry.
            let _ = table.close(fd as u32);
            0
        }
        Some(crate::vfd::Vfd::File(_)) => {
            if let Some(real_fd) = table.close_file_fd(fd as u32) {
                unsafe { real_close(real_fd); }
            }
            0
        }
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
        let ptr = crate::executor::get_current_executor();
        if !ptr.is_null() && unsafe { (*ptr).vprocs.contains_key(&(pid as u32)) } {
            // Virtual process — queue the signal for delivery at next schedule
            unsafe {
                if let Some(co) = (*ptr).vprocs.get_mut(&(pid as u32)) {
                    co.pending_signals.push(sig);
                }
            }
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
        let ptr = crate::executor::get_current_executor();
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
        let ptr = crate::executor::get_current_executor();
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

#[no_mangle]
pub extern "C" fn setsid() -> c_int {
    if enabled() {
        if let Some(vpid) = current_vpid() {
            return vpid as c_int;
        }
    }
    unsafe {
        let f: extern "C" fn() -> c_int = std::mem::transmute(real("setsid\0"));
        f()
    }
}

#[no_mangle]
pub extern "C" fn getpgrp() -> c_int {
    if enabled() {
        if let Some(vpid) = current_vpid() {
            return vpid as c_int;
        }
    }
    unsafe {
        let f: extern "C" fn() -> c_int = std::mem::transmute(real("getpgrp\0"));
        f()
    }
}

#[no_mangle]
pub extern "C" fn tcsetpgrp(fd: c_int, pgid: c_int) -> c_int {
    if enabled() {
        if let Some(vpid) = current_vpid() {
            if crate::vfd::get_table(vpid)
                .and_then(|t| t.get(fd as u32))
                .map(|vfd| match vfd {
                    crate::vfd::Vfd::Real(_) => false,
                    _ => true,
                })
                .unwrap_or(false)
            {
                return 0;
            }
        }
    }
    unsafe {
        let f: extern "C" fn(c_int, c_int) -> c_int = std::mem::transmute(real("tcsetpgrp\0"));
        f(fd, pgid)
    }
}

#[no_mangle]
pub extern "C" fn tcgetpgrp(fd: c_int) -> c_int {
    if enabled() {
        if let Some(vpid) = current_vpid() {
            if crate::vfd::get_table(vpid)
                .and_then(|t| t.get(fd as u32))
                .map(|vfd| match vfd {
                    crate::vfd::Vfd::Real(_) => false,
                    _ => true,
                })
                .unwrap_or(false)
            {
                return vpid as c_int;
            }
        }
    }
    unsafe {
        let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("tcgetpgrp\0"));
        f(fd)
    }
}

// ---------------------------------------------------------------------------
// raise()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn raise(sig: c_int) -> c_int {
    if enabled() {
        let vpid = current_vpid();
        if vpid.is_some() {
            let ptr = crate::executor::get_current_executor();
            if !ptr.is_null() {
                unsafe {
                    if let Some(co) = (*ptr).vprocs.get_mut(&vpid.unwrap()) {
                        match sig {
                            libc::SIGKILL | libc::SIGTERM => {
                                crate::executor::vproc_exit_with_code(128 + sig);
                                unreachable!()
                            }
                            _ => co.pending_signals.push(sig),
                        }
                    }
                }
                return 0;
            }
        }
    }
    // Not in a coroutine or not enabled — pass through to real raise
    unsafe {
        let f: extern "C" fn(c_int) -> c_int = std::mem::transmute(real("raise\0"));
        f(sig)
    }
}

// ---------------------------------------------------------------------------
// chdir() / getcwd()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn chdir(path: *const c_char) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(*const c_char) -> c_int =
                std::mem::transmute(real("chdir\0"));
            return f(path);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*const c_char) -> c_int =
                std::mem::transmute(real("chdir\0"));
            return f(path);
        }
    };

    let path_str = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy();

    let ptr = crate::executor::get_current_executor();
    if ptr.is_null() {
        unsafe {
            let f: extern "C" fn(*const c_char) -> c_int =
                std::mem::transmute(real("chdir\0"));
            return f(path);
        }
    }

    unsafe {
        let ex = &mut *ptr;
        if let Some(co) = ex.vprocs.get_mut(&vpid) {
            let resolved = match &co.cwd {
                Some(cwd) => resolve_path(cwd, &path_str),
                None => resolve_path(&process_cwd(), &path_str),
            };
            let c_resolved = match std::ffi::CString::new(resolved.clone()) {
                Ok(s) => s,
                Err(_) => {
                    *libc::__errno() = libc::EINVAL;
                    return -1;
                }
            };
            // Call real chdir so that real fork() children inherit the correct cwd.
            // Cooperative scheduling guarantees only one coroutine runs at a time,
            // so the real process cwd always matches the currently-running coroutine's cwd.
            let f: extern "C" fn(*const c_char) -> c_int =
                std::mem::transmute(real("chdir\0"));
            let ret = f(c_resolved.as_ptr());
            if ret == 0 {
                co.cwd = Some(resolved);
            }
            return ret;
        }
    }

    unsafe {
        let f: extern "C" fn(*const c_char) -> c_int =
            std::mem::transmute(real("chdir\0"));
        f(path)
    }
}

#[no_mangle]
pub extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(*mut c_char, usize) -> *mut c_char =
                std::mem::transmute(real("getcwd\0"));
            return f(buf, size);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*mut c_char, usize) -> *mut c_char =
                std::mem::transmute(real("getcwd\0"));
            return f(buf, size);
        }
    };

    let ptr = crate::executor::get_current_executor();
    if !ptr.is_null() {
        unsafe {
            if let Some(co) = (*ptr).vprocs.get(&vpid) {
                if let Some(ref cwd) = co.cwd {
                    let bytes = cwd.as_bytes();
                    if bytes.len() + 1 > size {
                        *libc::__errno() = libc::ERANGE;
                        return std::ptr::null_mut();
                    }
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
                    *buf.add(bytes.len()) = 0;
                    return buf;
                }
            }
        }
    }

    // No per-coroutine cwd — fall back to real getcwd
    unsafe {
        let f: extern "C" fn(*mut c_char, usize) -> *mut c_char =
            std::mem::transmute(real("getcwd\0"));
        f(buf, size)
    }
}

// ---------------------------------------------------------------------------
// open() / openat() / creat()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(*const c_char, c_int, c_int) -> c_int =
                std::mem::transmute(real("open\0"));
            return f(path, flags, mode);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*const c_char, c_int, c_int) -> c_int =
                std::mem::transmute(real("open\0"));
            return f(path, flags, mode);
        }
    };

    // Resolve relative paths against per-coroutine cwd
    let resolved_path = match get_cwd() {
        Some(ref cwd) => {
            let path_str = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy();
            if path_str.starts_with('/') {
                None // absolute path, use as-is
            } else {
                match std::ffi::CString::new(resolve_path(cwd, &path_str)) {
                    Ok(s) => Some(s),
                    Err(_) => {
                        unsafe { *libc::__errno() = libc::EINVAL; }
                        return -1;
                    }
                }
            }
        }
        None => None,
    };
    let open_path = resolved_path.as_ref()
        .map(|cs| cs.as_ptr())
        .unwrap_or(path);

    let real_fd = unsafe {
        let f: extern "C" fn(*const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(real("open\0"));
        f(open_path, flags, mode)
    };
    if real_fd < 0 {
        return real_fd;
    }

    let cloexec = (flags & libc::O_CLOEXEC) != 0;
    let table = crate::vfd::get_or_create_table(vpid);
    table.insert_file(real_fd, cloexec) as c_int
}

#[no_mangle]
pub extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int =
                std::mem::transmute(real("openat\0"));
            return f(dirfd, path, flags, mode);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int =
                std::mem::transmute(real("openat\0"));
            return f(dirfd, path, flags, mode);
        }
    };

    // Resolve relative paths against per-coroutine cwd when dirfd == AT_FDCWD
    let at_fdcwd: c_int = -100; // libc::AT_FDCWD
    let resolved_path = if dirfd == at_fdcwd {
        match get_cwd() {
            Some(ref cwd) => {
                let path_str = unsafe { std::ffi::CStr::from_ptr(path) }.to_string_lossy();
                if path_str.starts_with('/') {
                    None
                } else {
                    match std::ffi::CString::new(resolve_path(cwd, &path_str)) {
                        Ok(s) => Some(s),
                        Err(_) => {
                            unsafe { *libc::__errno() = libc::EINVAL; }
                            return -1;
                        }
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };
    let open_path = resolved_path.as_ref()
        .map(|cs| cs.as_ptr())
        .unwrap_or(path);

    let real_fd = unsafe {
        let f: extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(real("openat\0"));
        f(dirfd, open_path, flags, mode)
    };
    if real_fd < 0 {
        return real_fd;
    }

    let cloexec = (flags & libc::O_CLOEXEC) != 0;
    let table = crate::vfd::get_or_create_table(vpid);
    table.insert_file(real_fd, cloexec) as c_int
}

#[no_mangle]
pub extern "C" fn creat(path: *const c_char, mode: c_int) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(*const c_char, c_int) -> c_int =
                std::mem::transmute(real("creat\0"));
            return f(path, mode);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(*const c_char, c_int) -> c_int =
                std::mem::transmute(real("creat\0"));
            return f(path, mode);
        }
    };

    let real_fd = unsafe {
        let f: extern "C" fn(*const c_char, c_int) -> c_int =
            std::mem::transmute(real("creat\0"));
        f(path, mode)
    };
    if real_fd < 0 {
        return real_fd;
    }

    let table = crate::vfd::get_or_create_table(vpid);
    table.insert_file(real_fd, false) as c_int
}

// ---------------------------------------------------------------------------
// fstat() / lseek()
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(c_int, *mut libc::stat) -> c_int =
                std::mem::transmute(real("fstat\0"));
            return f(fd, buf);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, *mut libc::stat) -> c_int =
                std::mem::transmute(real("fstat\0"));
            return f(fd, buf);
        }
    };

    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => unsafe {
            let f: extern "C" fn(c_int, *mut libc::stat) -> c_int =
                std::mem::transmute(real("fstat\0"));
            return f(fd, buf);
        }
    };

    match table.get(fd as u32) {
        Some(crate::vfd::Vfd::Real(real_fd)) => unsafe {
            let f: extern "C" fn(c_int, *mut libc::stat) -> c_int =
                std::mem::transmute(real("fstat\0"));
            f(*real_fd, buf)
        },
        Some(crate::vfd::Vfd::File(file_ref)) => unsafe {
            let f: extern "C" fn(c_int, *mut libc::stat) -> c_int =
                std::mem::transmute(real("fstat\0"));
            f(file_ref.real_fd, buf)
        },
        Some(_) => {
            unsafe { *libc::__errno() = libc::ESPIPE; }
            -1
        },
        None => {
            unsafe { *libc::__errno() = libc::EBADF; }
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn lseek(fd: c_int, offset: isize, whence: c_int) -> isize {
    if !enabled() || is_real_fork_child() {
        unsafe {
            let f: extern "C" fn(c_int, isize, c_int) -> isize =
                std::mem::transmute(real("lseek\0"));
            return f(fd, offset, whence);
        }
    }
    let vpid = match current_vpid() {
        Some(p) => p,
        None => unsafe {
            let f: extern "C" fn(c_int, isize, c_int) -> isize =
                std::mem::transmute(real("lseek\0"));
            return f(fd, offset, whence);
        }
    };

    let table = match crate::vfd::get_table(vpid) {
        Some(t) => t,
        None => unsafe {
            let f: extern "C" fn(c_int, isize, c_int) -> isize =
                std::mem::transmute(real("lseek\0"));
            return f(fd, offset, whence);
        }
    };

    match table.get(fd as u32) {
        Some(crate::vfd::Vfd::Real(real_fd)) => unsafe {
            let f: extern "C" fn(c_int, isize, c_int) -> isize =
                std::mem::transmute(real("lseek\0"));
            f(*real_fd, offset, whence)
        },
        Some(crate::vfd::Vfd::File(file_ref)) => unsafe {
            let f: extern "C" fn(c_int, isize, c_int) -> isize =
                std::mem::transmute(real("lseek\0"));
            f(file_ref.real_fd, offset, whence)
        },
        Some(_) => {
            unsafe { *libc::__errno() = libc::ESPIPE; }
            -1
        },
        None => {
            unsafe { *libc::__errno() = libc::EBADF; }
            -1
        }
    }
}
