//! Coroutine abstraction backed by minicoro.
//!
//! Each Coroutine wraps a minicoro `mco_coro*` and provides Rust-safe
//! methods for lifecycle management. The minicoro library handles all
//! aarch64 assembly context switching internally.

use std::os::raw::{c_int, c_void};
use std::ptr;


pub type VPid = u32;

/// I/O wait state for a coroutine that yielded waiting for fd readiness.
pub struct IoWait {
    pub fds: Vec<(c_int, i16)>, // (real_fd, poll_events e.g. POLLIN/POLLOUT)
    pub deadline: Option<std::time::Instant>, // None = wait indefinitely
}

const DEFAULT_STACK_SIZE: usize = 2 * 1024 * 1024; // 2 MiB

// minicoro coroutine states (mirrors mco_state enum in minicoro.h).
#[allow(dead_code)]
const MCO_DEAD: i32 = 0;
#[allow(dead_code)]
const MCO_NORMAL: i32 = 1;
#[allow(dead_code)]
const MCO_RUNNING: i32 = 2;
#[allow(dead_code)]
const MCO_SUSPENDED: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Ready,
    Running,
    Done,
}

// minicoro opaque struct
#[repr(C)]
pub struct McoCoro {
    _opaque: [u8; 0],
}

#[repr(C)]
struct McoDesc {
    func: extern "C" fn(*mut McoCoro),
    user_data: *mut c_void,
    alloc_cb: extern "C" fn(usize, *mut c_void) -> *mut c_void,
    dealloc_cb: extern "C" fn(*mut c_void, usize, *mut c_void),
    allocator_data: *mut c_void,
    storage_size: usize,
    coro_size: usize,
    stack_size: usize,
}

extern "C" {
    fn mco_desc_init(func: extern "C" fn(*mut McoCoro), stack_size: usize) -> McoDesc;
    fn mco_create(out_co: *mut *mut McoCoro, desc: *mut McoDesc) -> i32;
    fn mco_destroy(co: *mut McoCoro) -> i32;
    fn mco_resume(co: *mut McoCoro) -> i32;
    fn mco_yield(co: *mut McoCoro) -> i32;
    fn mco_status(co: *mut McoCoro) -> i32;
    #[allow(dead_code)]
    fn mco_running() -> *mut McoCoro;
    fn mco_get_user_data(co: *mut McoCoro) -> *mut c_void;
    fn mco_set_user_data(co: *mut McoCoro, data: *mut c_void);
    fn mco_create_with_elf_entry(
        out_co: *mut *mut McoCoro,
        entry: extern "C" fn(),
        elf_sp: *mut c_void,
        stack_base: *mut c_void,
        stack_size: usize,
    ) -> i32;
    fn mco_fork_from(parent: *mut McoCoro, out_co: *mut *mut McoCoro) -> i32;
    #[allow(dead_code)]
    fn mco_set_stack(co: *mut McoCoro, stack_base: *mut c_void, stack_size: usize);
}

/// Per-coroutine metadata stored via user_data.
struct CoroUserdata {
    pub exit_code: i32,
    /// The closure to execute. Set before first resume, consumed by trampoline.
    pub closure: Option<Box<Box<dyn FnOnce()>>>,
}

/// minicoro trampoline: called when a standard coroutine starts.
/// Reads the closure from user_data and executes it.
extern "C" fn mco_trampoline(co: *mut McoCoro) {
    let ud_ptr = unsafe { mco_get_user_data(co) };
    if ud_ptr.is_null() {
        crate::executor::vproc_exit_with_code(128);
        return;
    }
    let ud = unsafe { &mut *(ud_ptr as *mut CoroUserdata) };
    let f = ud.closure.take().expect("mco_trampoline: no closure in userdata");
    let f: Box<dyn FnOnce()> = *f;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f();
    }));

    let code = if result.is_err() { 134 } else { 0 };
    crate::executor::vproc_exit_with_code(code);
}

/// A virtual process (coroutine) backed by minicoro.
pub struct Coroutine {
    pub id: VPid,
    pub ppid: VPid,
    co: *mut McoCoro,
    pub state: State,
    pub exit_code: i32,
    pub is_fork_child: bool,
    pub fork_child_pid: u32,
    pub c_strings: Vec<*mut u8>,
    pub mapped_regions: Vec<(usize, usize)>,
    pub pending_signals: Vec<i32>,
    pub cwd: Option<String>,
    pub binary_path: Option<String>,
    pub io_wait: Option<IoWait>,
    stack_alloc: Option<(*mut u8, usize)>,
}

impl Coroutine {
    /// Create a closure-backed coroutine.
    pub fn new(id: VPid, f: Box<dyn FnOnce()>) -> Self {
        let ud = Box::into_raw(Box::new(CoroUserdata {
            exit_code: 0,
            closure: Some(Box::new(f)),
        }));

        let mut co_ptr: *mut McoCoro = ptr::null_mut();
        let mut desc = unsafe { mco_desc_init(mco_trampoline, DEFAULT_STACK_SIZE) };
        desc.user_data = ud as *mut c_void;
        let rc = unsafe { mco_create(&mut co_ptr, &mut desc as *mut _) };
        if rc != 0 {
            eprintln!("vproc: mco_create failed: {}", rc);
            std::process::abort();
        }

        Coroutine {
            id,
            ppid: 0,
            co: co_ptr,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: false,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: None,
            binary_path: None,
            io_wait: None,
            stack_alloc: None,
        }
    }

    /// Create a coroutine backed by a loaded ELF binary.
    ///
    /// Sets up the stack with argc/argv/envp/auxv and creates a minicoro
    /// coroutine that will jump directly to the ELF entry point.
    ///
    /// # Safety
    ///
    /// `entry` must be a valid function pointer. `stack_base` must point to
    /// a valid memory region of at least `stack_size` bytes.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new_elf(
        id: VPid,
        entry: usize,
        stack_base: *mut u8,
        stack_size: usize,
        argc: usize,
        argv: Vec<*const u8>,
        envp: Vec<*const u8>,
        auxv: Vec<[u64; 2]>,
    ) -> Self {
        let stack_top = unsafe { stack_base.add(stack_size) };

        let argv_size = (argv.len() + 1) * 8;
        let envp_size = (envp.len() + 1) * 8;
        let auxv_size = auxv.len() * 16;
        let elf_data_size = (8 + argv_size + envp_size + auxv_size + 15) & !15;

        let elf_data_bottom = unsafe { stack_top.sub(elf_data_size) };
        unsafe {
            let mut sp = elf_data_bottom as *mut u64;
            ptr::write_unaligned(sp, argc as u64);
            sp = sp.add(1);
            for &arg in &argv {
                ptr::write_unaligned(sp, arg as u64);
                sp = sp.add(1);
            }
            ptr::write_unaligned(sp, 0);
            sp = sp.add(1);
            for &env in &envp {
                ptr::write_unaligned(sp, env as u64);
                sp = sp.add(1);
            }
            ptr::write_unaligned(sp, 0);
            sp = sp.add(1);
            for &[kind, value] in &auxv {
                ptr::write_unaligned(sp, kind);
                ptr::write_unaligned(sp.add(1), value);
                sp = sp.add(2);
            }
        }

        let mut co_ptr: *mut McoCoro = ptr::null_mut();
        let entry_fn: extern "C" fn() = unsafe { std::mem::transmute(entry) };
        let rc = unsafe {
            mco_create_with_elf_entry(
                &mut co_ptr,
                entry_fn,
                elf_data_bottom as *mut c_void,
                stack_base as *mut c_void,
                stack_size,
            )
        };
        if rc != 0 {
            eprintln!("vproc: mco_create_with_elf_entry failed: {}", rc);
            std::process::abort();
        }

        let ud = Box::into_raw(Box::new(CoroUserdata { exit_code: 0, closure: None }));
        unsafe { mco_set_user_data(co_ptr, ud as *mut c_void) };

        Coroutine {
            id,
            ppid: 0,
            co: co_ptr,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: false,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: None,
            binary_path: None,
            io_wait: None,
            stack_alloc: Some((stack_base, stack_size)),
        }
    }

    /// Resume (or start) the coroutine. Updates state after return.
    pub fn resume(&mut self) {
        self.state = State::Running;
        unsafe {
            mco_resume(self.co);
            if self.state != State::Done {
                if mco_status(self.co) == MCO_DEAD {
                    self.state = State::Done;
                } else {
                    self.state = State::Ready;
                }
            }
        }
    }

    pub fn r#yield(&mut self) {
        self.state = State::Ready;
        unsafe { mco_yield(self.co); }
    }

    /// Mark the coroutine as finished with the given exit code.
    pub fn set_done(&mut self, code: i32) {
        self.state = State::Done;
        self.exit_code = code;
        unsafe {
            let ud = mco_get_user_data(self.co) as *mut CoroUserdata;
            (*ud).exit_code = code;
            drop((*ud).closure.take());
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    pub fn mco(&self) -> *mut McoCoro {
        self.co
    }

    /// Fork a new coroutine from an existing one, copying the minicoro context.
    ///
    /// # Safety
    ///
    /// The parent coroutine must be in a valid suspended state with a live
    /// minicoro handle. The caller must ensure the parent's stack remains valid.
    pub unsafe fn fork_from(child_id: VPid, parent: &Coroutine) -> Self {
        let mut child_co: *mut McoCoro = ptr::null_mut();
        let rc = mco_fork_from(parent.co, &mut child_co);
        assert!(rc == 0, "mco_fork_from failed: {}", rc);

        let ud = Box::into_raw(Box::new(CoroUserdata { exit_code: 0, closure: None }));
        mco_set_user_data(child_co, ud as *mut c_void);

        Coroutine {
            id: child_id,
            ppid: parent.id,
            co: child_co,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: true,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: parent.cwd.clone(),
            binary_path: None,
            io_wait: None,
            stack_alloc: None,
        }
    }
}

impl Drop for Coroutine {
    fn drop(&mut self) {
        for ptr in self.c_strings.drain(..) {
            unsafe { let _ = std::ffi::CString::from_raw(ptr.cast()); }
        }
        for &(base, size) in &self.mapped_regions {
            unsafe { libc::munmap(base as *mut _, size); }
        }
        if let Some((base, size)) = self.stack_alloc {
            unsafe { crate::vexec::free_elf_stack(base, size); }
        }
        unsafe {
            let ud_ptr = mco_get_user_data(self.co);
            if !ud_ptr.is_null() {
                drop(Box::from_raw(ud_ptr as *mut CoroUserdata));
            }
            mco_destroy(self.co);
        }
    }
}
