use std::collections::HashMap;
use std::os::raw::{c_char, c_int};

type VPid = u32;

struct ChildInfo {
    done: bool,
    exit_code: i32,
}

struct VirtualProc {
    next_pid: VPid,
    current_pid: VPid,
    children: HashMap<VPid, ChildInfo>,
}

thread_local! {
    static VP: std::cell::UnsafeCell<VirtualProc> = std::cell::UnsafeCell::new(VirtualProc {
        next_pid: 10000,
        current_pid: 0,
        children: HashMap::new(),
    });
}

/// Check if vproc is active (via VPROC=1 env var).
/// If not set, all intercepted functions pass through to real libc.
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
unsafe fn real(sym: &'static str) -> *mut std::ffi::c_void {
    // RTLD_NEXT = -1 on Linux/Android (as a void* cast)
    let rtld_next = -1isize as *mut std::ffi::c_void;
    let ptr = libc::dlsym(rtld_next, sym.as_ptr() as *const c_char);
    if ptr.is_null() {
        eprintln!("vproc: cannot resolve {}", sym);
        libc::_exit(99);
    }
    ptr
}

fn vp() -> &'static mut VirtualProc {
    unsafe { &mut *VP.with(|e| e.get()) }
}

/// Call the real libc fork.
unsafe fn real_fork() -> c_int {
    let f = std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn() -> c_int>(real("fork\0"));
    f()
}

unsafe fn real_execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int {
    let f = std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int>(real("execve\0"));
    f(path, argv, envp)
}

unsafe fn real_waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    let f = std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn(c_int, *mut c_int, c_int) -> c_int>(real("waitpid\0"));
    f(pid, status, options)
}

unsafe fn real_exit(code: c_int) -> ! {
    let f = std::mem::transmute::<*mut std::ffi::c_void, unsafe extern "C" fn(c_int) -> !>(real("_exit\0"));
    f(code)
}

// ---------------------------------------------------------------------------
// fork()
// ---------------------------------------------------------------------------

/// Intercepted fork().
///
/// Phase 2 strategy: allocate a virtual PID and do a real fork.
/// The child gets the virtual PID. The parent tracks the child.
/// Phase 3 will eliminate the real fork entirely.
#[no_mangle]
pub extern "C" fn fork() -> c_int {
    if !enabled() { return unsafe { real_fork() }; }

    let v = vp();
    let child_vpid = v.next_pid;
    v.next_pid += 1;

    let real_pid = unsafe { libc::fork() };

    if real_pid == 0 {
        v.current_pid = child_vpid;
        0
    } else if real_pid > 0 {
        v.children.insert(child_vpid, ChildInfo { done: false, exit_code: 0 });
        child_vpid as c_int
    } else {
        -1
    }
}

// ---------------------------------------------------------------------------
// execve()
// ---------------------------------------------------------------------------

/// Type signature for the real execve.
type RealExecve = unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int;

/// Intercepted execve().
///
/// Phase 2: pass through to real execve.
/// Phase 3: load ELF in-process via coroutine.
#[no_mangle]
pub extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if !enabled() { return unsafe { real_execve(path, argv, envp) }; }
    unsafe {
        let f: RealExecve = std::mem::transmute(real("execve\0"));
        f(path, argv, envp)
    }
}

// ---------------------------------------------------------------------------
// waitpid()
// ---------------------------------------------------------------------------

type RealWaitpid = unsafe extern "C" fn(c_int, *mut c_int, c_int) -> c_int;

#[no_mangle]
pub extern "C" fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int {
    if !enabled() { return unsafe { real_waitpid(pid, status, options) }; }
    let v = vp();
    let vpid = pid as VPid;

    // If this is one of our virtual PIDs, wait for the real child (-1 = any)
    if vpid >= 10000 && v.children.contains_key(&vpid) {
        // Find any finished real child
        let f: RealWaitpid = unsafe { std::mem::transmute(real("waitpid\0")) };
        let ret = unsafe { f(-1, status, options) };
        if ret > 0 {
            if let Some(info) = v.children.get_mut(&vpid) {
                info.done = true;
                info.exit_code = if !status.is_null() { unsafe { *status >> 8 } } else { 0 };
            }
            return vpid as c_int;
        }
        return ret;
    }

    // Not a virtual PID
    let f: RealWaitpid = unsafe { std::mem::transmute(real("waitpid\0")) };
    unsafe { f(pid, status, options) }
}

// ---------------------------------------------------------------------------
// _exit() / exit()
// ---------------------------------------------------------------------------

type RealExit = unsafe extern "C" fn(c_int) -> !;

#[no_mangle]
pub extern "C" fn _exit(code: c_int) -> ! {
    if !enabled() { unsafe { real_exit(code); } }
    let v = vp();
    if v.current_pid > 0 {
        if let Some(info) = v.children.get_mut(&v.current_pid) {
            info.done = true;
            info.exit_code = code;
        }
    }
    unsafe {
        let f: RealExit = std::mem::transmute(real("_exit\0"));
        f(code);
    }
}

#[no_mangle]
pub extern "C" fn exit(code: c_int) -> ! {
    if !enabled() { unsafe { real_exit(code); } }
    unsafe {
        let f: RealExit = std::mem::transmute(real("_exit\0"));
        f(code);
    }
}
