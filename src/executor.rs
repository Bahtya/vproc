use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::collections::VecDeque;

use crate::arch::aarch64::context_switch;
use crate::coroutine::{Coroutine, State, VPid};

thread_local! {
    pub static EXECUTOR: UnsafeCell<Executor> = UnsafeCell::new(Executor::new());
}

const MAIN_VPID: VPid = 0;

pub struct Executor {
    vprocs: HashMap<VPid, Coroutine>,
    ready_queue: VecDeque<VPid>,
    next_pid: VPid,
    current: Option<VPid>,
    main_sp: *mut u8,
    switch_count: u64,
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
        while self.vprocs.values().any(|c| !c.is_done()) {
            self.r#yield();
        }
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

/// Called from assembly trampoline when a coroutine finishes.
/// Marks the coroutine as Done and yields back.
#[no_mangle]
pub extern "C" fn vproc_exit() {
    let ex = unsafe { &mut *EXECUTOR.with(|e| e.get()) };
    let pid = match ex.current {
        Some(p) => p,
        None => return,
    };
    if let Some(co) = ex.vprocs.get_mut(&pid) {
        co.state = State::Done;
    }
    // Don't re-queue — just schedule the next one
    ex.schedule();
}

/// Public API: yield from current coroutine
pub fn do_yield() {
    let ex = unsafe { &mut *EXECUTOR.with(|e| e.get()) };
    ex.r#yield();
}

fn sp_ptr(p: &mut *mut u8) -> *mut *mut u8 {
    p as *mut *mut u8
}
