use std::alloc::{alloc, dealloc, Layout};
use std::ptr;

const STACK_SIZE: usize = 2 * 1024 * 1024; // 2 MiB

pub type VPid = u32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Done,
}

pub struct Coroutine {
    pub id: VPid,
    pub ppid: VPid,
    pub sp: *mut u8,
    pub stack_base: *mut u8,
    pub stack_size: usize,
    pub state: State,
    pub exit_code: i32,
    pub is_fork_child: bool,
    pub fork_child_pid: u32,
    /// C strings allocated for argv/envp. Freed on Drop.
    pub c_strings: Vec<*mut u8>,
    /// mmap'd regions to munmap on Drop (base, size).
    pub mapped_regions: Vec<(usize, usize)>,
    /// Queued signals to deliver before next context switch.
    pub pending_signals: Vec<i32>,
    /// Per-coroutine working directory. None = use process cwd.
    pub cwd: Option<String>,
    /// Path of the cached binary this coroutine is executing.
    /// Used to track active users for auto-dlclose when the coroutine exits.
    pub binary_path: Option<String>,
}

// Assembly trampoline: vproc_switch restores x19=f_ptr then `ret` jumps here.
// We move x19→x0 and call the Rust entry function.
std::arch::global_asm!(
    ".text",
    ".align 2",
    ".global __vproc_trampoline",
    ".type __vproc_trampoline, @function",
    "__vproc_trampoline:",
    "mov     x0, x19",          // f_ptr → first arg
    "bl      __vproc_entry",    // call Rust entry
    "bl      vproc_exit",       // never returns (yields back to executor)
    "brk     #1",               // unreachable
    ".size __vproc_trampoline, . - __vproc_trampoline",
);

// Trampoline for calling main() directly in a coroutine.
// x19 = main_addr, x20 = argc, x21 = argv, x22 = envp
std::arch::global_asm!(
    ".text",
    ".align 2",
    ".global __vproc_main_call",
    ".type __vproc_main_call, @function",
    "__vproc_main_call:",
    "mov    x0, x20",          // argc
    "mov    x1, x21",          // argv
    "mov    x2, x22",          // envp
    "blr    x19",              // call main(argc, argv, envp)
    "bl     vproc_exit_with_code", // x0 = main's return value
    "brk    #1",               // unreachable
    ".size __vproc_main_call, . - __vproc_main_call",
);

extern "C" {
    fn __vproc_trampoline();
    fn __vproc_elf_entry();
    pub fn __vproc_main_call();
}

/// Rust entry function called by the assembly trampoline.
/// `f_ptr` is the raw pointer from `Box::into_raw()`.
#[no_mangle]
unsafe extern "C" fn __vproc_entry(f_ptr: *mut u8) {
    // f_ptr is Box<Box<dyn FnOnce()>> — a thin pointer
    let outer = Box::from_raw(f_ptr as *mut Box<dyn FnOnce()>);
    let f: Box<dyn FnOnce()> = *outer;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f();
    }));

    if result.is_err() {
        // Panic caught — exit with 134 (128 + SIGABRT). This context-switches
        // away and never returns, so the trampoline's `bl vproc_exit` is never reached.
        crate::executor::vproc_exit_with_code(134);
    }
}

impl Coroutine {
    pub fn new(id: VPid, f: Box<dyn FnOnce()>) -> Self {
        let layout = Layout::from_size_align(STACK_SIZE, 16).unwrap();
        let stack_base = unsafe { alloc(layout) };
        assert!(!stack_base.is_null(), "stack alloc failed for coroutine {}", id);

        let stack_top = unsafe { stack_base.add(STACK_SIZE) };

        // Frame layout matching vproc_switch (160 bytes):
        //   sp+0:   x19, x20    ← x19 = f_ptr
        //   sp+16:  x21, x22
        //   sp+32:  x23, x24
        //   sp+48:  x25, x26
        //   sp+64:  x27, x28
        //   sp+80:  x29, x30    ← x30 = __vproc_trampoline
        //   sp+96:  d8, d9
        //   sp+112: d10, d11
        //   sp+128: d12, d13
        //   sp+144: d14, d15
        let frame_size: usize = 160;
        // SAFETY: frame_size (160) << STACK_SIZE (2 MiB), so this is in bounds.
        let sp_init = unsafe { stack_top.sub(frame_size) };
        // Double-box: Box<Box<dyn FnOnce()>> gives a thin pointer
        let f_ptr = Box::into_raw(Box::new(f)) as *mut u8;

        // SAFETY: sp_init is within a freshly allocated, exclusively owned stack.
        // Offsets are within frame_size bytes of sp_init, all in bounds.
        unsafe {
            ptr::write_unaligned(sp_init as *mut u64, f_ptr as u64);
            ptr::write_unaligned(sp_init.add(8) as *mut u64, 0);

            // x21-x28: zero
            for off in [16usize, 32, 48, 64] {
                ptr::write_unaligned(sp_init.add(off) as *mut u64, 0);
                ptr::write_unaligned(sp_init.add(off + 8) as *mut u64, 0);
            }

            // x29(fp) = 0, x30(lr) = trampoline
            ptr::write_unaligned(sp_init.add(80) as *mut u64, 0);
            ptr::write_unaligned(sp_init.add(88) as *mut u64, __vproc_trampoline as *const () as usize as u64);

            // d8-d15: zero
            for i in 0..8 {
                ptr::write_unaligned(sp_init.add(96 + i * 8) as *mut u64, 0);
            }
        }

        Coroutine {
            id,
            ppid: 0,
            sp: sp_init,
            stack_base,
            stack_size: STACK_SIZE,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: false,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: None,
            binary_path: None,
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Create a Coroutine from pre-initialized stack state.
    /// The caller sets up the switch frame (callee-saved registers) directly.
    pub fn from_raw_parts(
        id: VPid,
        sp: *mut u8,
        stack_base: *mut u8,
        stack_size: usize,
    ) -> Self {
        Coroutine {
            id,
            ppid: 0,
            sp,
            stack_base,
            stack_size,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: false,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: None,
            binary_path: None,
        }
    }

    /// Create a coroutine whose stack is set up for ELF entry.
    ///
    /// When vproc_switch restores this frame, it jumps to `__vproc_elf_entry`
    /// which sets sp = x20 (ELF stack data) and jumps to x19 (entry point).
    ///
    /// Stack layout (growing downward):
    ///   [vproc_switch frame: 160 bytes] ← initial sp
    ///   [ELF data: argc, argv[], NULL, envp[], NULL, auxv[]] ← x20
    pub fn new_elf(
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

        // Calculate ELF data area size
        let argv_size = (argv.len() + 1) * 8;
        let envp_size = (envp.len() + 1) * 8;
        let auxv_size = auxv.len() * 16;
        let elf_data_size = 8 + argv_size + envp_size + auxv_size;
        let elf_data_size = (elf_data_size + 15) & !15; // align 16

        // ELF data starts below stack_top
        let elf_data_top = stack_top;
        let elf_data_bottom = unsafe { elf_data_top.sub(elf_data_size) };

        // Write ELF data to stack
        unsafe {
            let mut sp = elf_data_bottom as *mut u64;

            // argc
            ptr::write_unaligned(sp, argc as u64);
            sp = sp.add(1);

            // argv pointers
            for &arg in &argv {
                ptr::write_unaligned(sp, arg as u64);
                sp = sp.add(1);
            }
            ptr::write_unaligned(sp, 0); // NULL
            sp = sp.add(1);

            // envp pointers
            for &env in &envp {
                ptr::write_unaligned(sp, env as u64);
                sp = sp.add(1);
            }
            ptr::write_unaligned(sp, 0); // NULL
            sp = sp.add(1);

            // auxv entries
            for &[kind, value] in &auxv {
                ptr::write_unaligned(sp, kind);
                ptr::write_unaligned(sp.add(1), value);
                sp = sp.add(2);
            }
        }

        // vproc_switch frame below the ELF data
        let frame_size: usize = 160;
        let frame_sp = unsafe { elf_data_bottom.sub(frame_size) };

        unsafe {
            // x19 = entry point, x20 = ELF data sp
            ptr::write_unaligned(frame_sp as *mut u64, entry as u64);
            ptr::write_unaligned(frame_sp.add(8) as *mut u64, elf_data_bottom as u64);

            // x21-x28: zero
            for off in [16usize, 32, 48, 64] {
                ptr::write_unaligned(frame_sp.add(off) as *mut u64, 0);
                ptr::write_unaligned(frame_sp.add(off + 8) as *mut u64, 0);
            }

            // x29(fp) = 0, x30(lr) = __vproc_elf_entry
            ptr::write_unaligned(frame_sp.add(80) as *mut u64, 0);
            ptr::write_unaligned(
                frame_sp.add(88) as *mut u64,
                __vproc_elf_entry as *const () as usize as u64,
            );

            // d8-d15: zero
            for i in 0..8 {
                ptr::write_unaligned(frame_sp.add(96 + i * 8) as *mut u64, 0);
            }
        }

        Coroutine {
            id,
            ppid: 0,
            sp: frame_sp,
            stack_base,
            stack_size,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: false,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: None,
            binary_path: None,
        }
    }

    /// Create a child coroutine by copying the parent's stack (virtual fork).
    ///
    /// Copies the entire parent stack (including the saved register frame
    /// at Coroutine::sp). The child resumes at the same execution point
    /// as the parent when scheduled.
    ///
    /// # Safety: caller-saved register limitation
    ///
    /// `vproc_switch` only saves callee-saved registers (x19-x30, d8-d15).
    /// Any variable in a caller-saved register (x0-x18) at the fork point
    /// will have an undefined value in the child. This is safe only when
    /// the child immediately calls execve() (which replaces the entire
    /// execution context) or when the code between fork() return and the
    /// next function call does not depend on caller-saved register values.
    pub unsafe fn fork_from(child_id: VPid, parent: &Coroutine) -> Self {
        let layout = Layout::from_size_align(parent.stack_size, 16).unwrap();
        let child_stack_base = alloc(layout);
        assert!(!child_stack_base.is_null(), "fork stack alloc failed");

        ptr::copy_nonoverlapping(
            parent.stack_base,
            child_stack_base,
            parent.stack_size,
        );

        let sp_offset = parent.sp.offset_from(parent.stack_base) as usize;
        let child_sp = child_stack_base.add(sp_offset);

        // Remap frame pointers and stack references: scan all 8-byte values
        // in the child's stack that point into the parent's stack range and
        // adjust them to point to the equivalent location in the child's stack.
        let parent_base = parent.stack_base as usize;
        let parent_end = parent_base + parent.stack_size;
        let child_base = child_stack_base as usize;
        let delta = child_base as isize - parent_base as isize;

        // Scan from sp to stack_top (the active region).
        // Values below sp are uninitialized and don't need remapping.
        let scan_start = child_sp;
        let scan_end = child_stack_base.add(parent.stack_size);
        let mut p = scan_start as *mut u64;
        let end = scan_end as *mut u64;
        while p < end {
            let val = p.read();
            if val >= parent_base as u64 && val < parent_end as u64 {
                let remapped = (val as i64 + delta as i64) as u64;
                p.write(remapped);
            }
            p = p.add(1);
        }

        Coroutine {
            id: child_id,
            ppid: parent.id,
            sp: child_sp,
            stack_base: child_stack_base,
            stack_size: parent.stack_size,
            state: State::Ready,
            exit_code: 0,
            is_fork_child: true,
            fork_child_pid: 0,
            c_strings: Vec::new(),
            mapped_regions: Vec::new(),
            pending_signals: Vec::new(),
            cwd: parent.cwd.clone(),
            binary_path: None,
        }
    }
}

impl Drop for Coroutine {
    fn drop(&mut self) {
        // Free C strings allocated for argv/envp.
        for ptr in self.c_strings.drain(..) {
            unsafe { let _ = std::ffi::CString::from_raw(ptr.cast()); }
        }
        // Unmap mmap'd regions (e.g., PIE loader images).
        for &(base, size) in &self.mapped_regions {
            unsafe { libc::munmap(base as *mut _, size); }
        }
        let layout = Layout::from_size_align(self.stack_size, 16).unwrap();
        unsafe { dealloc(self.stack_base, layout) };
    }
}
