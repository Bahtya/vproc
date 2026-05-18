use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::arch::aarch64::context_switch;
use crate::coroutine::{Coroutine, State, VPid};

thread_local! {
    pub static EXECUTOR: UnsafeCell<Executor> = UnsafeCell::new(Executor::new());
}

/// Global pointer to the executor, set when block_on_all starts.
/// Survives TLS reinitialization (e.g. when __libc_init runs inside a dlopen'd binary).
static EXECUTOR_PTR: AtomicPtr<Executor> = AtomicPtr::new(std::ptr::null_mut());

/// Set the global executor pointer (called from block_on_all).
pub fn set_global_executor(ptr: *mut Executor) {
    EXECUTOR_PTR.store(ptr, Ordering::SeqCst);
}

/// Get the executor via the global pointer. Returns null if not set.
pub fn get_global_executor() -> *mut Executor {
    let ptr = EXECUTOR_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        return ptr;
    }
    // Fallback to thread-local
    EXECUTOR.with(|e| e.get())
}

const MAIN_VPID: VPid = 0;

pub struct Executor {
    pub vprocs: HashMap<VPid, Coroutine>,
    ready_queue: VecDeque<VPid>,
    next_pid: VPid,
    pub current: Option<VPid>,
    main_sp: *mut u8,
    switch_count: u64,
    pub children: HashMap<VPid, Vec<VPid>>,
    /// Saved lr (x30) value for fork LR restoration.
    /// Stored on the heap (Executor is heap-allocated via AtomicPtr),
    /// so it survives stack corruption from dlopen/__libc_init in other coroutines.
    pub saved_fork_lr: Option<u64>,
}

impl Executor {
    pub fn new() -> Self {
        Executor {
            vprocs: HashMap::new(),
            ready_queue: VecDeque::new(),
            next_pid: 1,
            current: None,
            main_sp: std::ptr::null_mut(),
            switch_count: 0,
            children: HashMap::new(),
            saved_fork_lr: None,
        }
    }

    pub fn spawn(&mut self, f: Box<dyn FnOnce()>) -> VPid {
        let pid = self.next_pid;
        self.next_pid += 1;
        let co = Coroutine::new(pid, f);
        self.vprocs.insert(pid, co);
        self.ready_queue.push_back(pid);
        pid
    }

    /// Spawn a coroutine at the front of the ready queue.
    /// Used for fork helpers that must run before any other coroutine
    /// to prevent scheduling interleaving that corrupts the parent's stack.
    pub fn spawn_front(&mut self, f: Box<dyn FnOnce()>) -> VPid {
        let pid = self.next_pid;
        self.next_pid += 1;
        let co = Coroutine::new(pid, f);
        self.vprocs.insert(pid, co);
        self.ready_queue.push_front(pid);
        pid
    }

    /// Spawn a coroutine that will execute a loaded ELF binary.
    pub fn spawn_elf(
        &mut self,
        entry: usize,
        stack_base: *mut u8,
        stack_size: usize,
        argc: usize,
        argv: Vec<*const u8>,
        envp: Vec<*const u8>,
        auxv: Vec<[u64; 2]>,
    ) -> VPid {
        let pid = self.next_pid;
        self.next_pid += 1;
        let co = Coroutine::new_elf(pid, entry, stack_base, stack_size, argc, argv, envp, auxv);
        self.vprocs.insert(pid, co);
        self.ready_queue.push_back(pid);
        pid
    }

    /// Create a fork child coroutine from the given parent's saved stack state.
    ///
    /// Allocates a new VPid, copies the parent's stack via `Coroutine::fork_from`,
    /// registers the parent-child relationship, and copies the fd table.
    ///
    /// Returns the child's VPid and writes it to the parent's `fork_child_pid` field.
    pub fn spawn_fork_child(&mut self, parent_pid: VPid) -> VPid {
        let child_id = self.next_pid;
        self.next_pid += 1;

        let child = unsafe {
            let parent_co = self.vprocs.get(&parent_pid).unwrap();
            Coroutine::fork_from(child_id, parent_co)
        };

        self.vprocs.insert(child_id, child);
        self.ready_queue.push_back(child_id);
        self.children.entry(parent_pid).or_default().push(child_id);
        crate::vfd::fork_fd_table(parent_pid, child_id);
        self.vprocs.get_mut(&parent_pid).unwrap().fork_child_pid = child_id;

        child_id
    }

    /// Returns the current coroutine's VPid, or None if on the main stack.
    pub fn current_pid(&self) -> Option<VPid> {
        self.current
    }

    /// Switch to the next ready coroutine, or back to main.
    fn schedule(&mut self) {
        let current_pid = self.current.unwrap_or(MAIN_VPID);

        let next = self.pick_next();

        match next {
            Some(next_pid) => {
                // Re-queue current if it's a coroutine and still alive
                if current_pid != MAIN_VPID {
                    let co = self.vprocs.get(&current_pid).unwrap();
                    if co.state == State::Running {
                        // Yield (not exit): put back in queue
                        self.vprocs.get_mut(&current_pid).unwrap().state = State::Ready;
                        self.ready_queue.push_back(current_pid);
                    }
                }

                let new_sp = self.vprocs.get(&next_pid).unwrap().sp;
                let old_sp_ptr = if current_pid == MAIN_VPID {
                    sp_ptr(&mut self.main_sp)
                } else {
                    sp_ptr(&mut self.vprocs.get_mut(&current_pid).unwrap().sp)
                };

                self.vprocs.get_mut(&next_pid).unwrap().state = State::Running;
                self.current = Some(next_pid);
                self.switch_count += 1;

                unsafe { context_switch(old_sp_ptr, new_sp) };
            }
            None => {
                // No ready coroutine, switch back to main
                if current_pid != MAIN_VPID {
                    self.current = None;
                    self.switch_count += 1;

                    let old_sp_ptr = sp_ptr(&mut self.vprocs.get_mut(&current_pid).unwrap().sp);
                    unsafe { context_switch(old_sp_ptr, self.main_sp) };
                }
            }
        }
    }

    pub fn r#yield(&mut self) {
        self.schedule();
    }

    pub fn block_on_all(&mut self) {
        set_global_executor(self as *mut Executor);
        while self.vprocs.values().any(|c| !c.is_done()) {
            self.r#yield();
        }
        // Clean up children entries for removed coroutines
        let done_pids: Vec<VPid> = self.vprocs.iter()
            .filter(|(_, co)| co.is_done())
            .map(|(&pid, _)| pid)
            .collect();
        for pid in &done_pids {
            self.children.remove(pid);
        }
        self.children.retain(|_, kids| {
            kids.retain(|k| self.vprocs.contains_key(k));
            !kids.is_empty()
        });
        self.vprocs.retain(|_, co| !co.is_done());
    }

    fn pick_next(&mut self) -> Option<VPid> {
        while let Some(pid) = self.ready_queue.pop_front() {
            if let Some(co) = self.vprocs.get(&pid) {
                if !co.is_done() {
                    return Some(pid);
                }
            }
        }
        None
    }

    pub fn switch_count(&self) -> u64 {
        self.switch_count
    }
}

/// Called from assembly trampoline when a coroutine finishes normally.
/// Marks the coroutine as Done and yields back.
#[no_mangle]
pub extern "C" fn vproc_exit() {
    vproc_exit_with_code(0);
}

/// Terminate the current coroutine with an exit code.
pub fn vproc_exit_with_code(code: i32) {
    let ex = unsafe { &mut *get_global_executor() };
    let pid = match ex.current {
        Some(p) => p,
        None => return,
    };
    if let Some(co) = ex.vprocs.get_mut(&pid) {
        co.state = State::Done;
        co.exit_code = code;
    }
    // Don't re-queue — just schedule the next one
    ex.schedule();
}

/// Get the exit code of a completed coroutine.
/// Returns None if the coroutine hasn't finished yet or doesn't exist.
pub fn get_exit_code(pid: VPid) -> Option<i32> {
    let ex = unsafe { &mut *get_global_executor() };
    ex.vprocs.get(&pid).and_then(|co| {
        if co.is_done() { Some(co.exit_code) } else { None }
    })
}

/// Check if a VPid is a child of the given parent.
pub fn is_child_of(parent: VPid, child: VPid) -> bool {
    let ex = unsafe { &mut *get_global_executor() };
    ex.children.get(&parent).map_or(false, |kids| kids.contains(&child))
}

/// Remove a child from the parent's children list (after waitpid reaps it).
pub fn reap_child(parent: VPid, child: VPid) {
    let ex = unsafe { &mut *get_global_executor() };
    if let Some(kids) = ex.children.get_mut(&parent) {
        kids.retain(|&k| k != child);
    }
}

/// Public API: yield from current coroutine
pub fn do_yield() {
    let ex = unsafe { &mut *get_global_executor() };
    ex.r#yield();
}

fn sp_ptr(p: &mut *mut u8) -> *mut *mut u8 {
    p as *mut *mut u8
}
