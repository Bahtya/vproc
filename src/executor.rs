use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::coroutine::{Coroutine, State, VPid};

thread_local! {
    pub static EXECUTOR: UnsafeCell<Executor> = UnsafeCell::new(Executor::new());
}

static EXECUTOR_PTR: AtomicPtr<Executor> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_global_executor(ptr: *mut Executor) {
    EXECUTOR_PTR.store(ptr, Ordering::SeqCst);
}

pub fn get_global_executor() -> *mut Executor {
    let ptr = EXECUTOR_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        return ptr;
    }
    EXECUTOR.with(|e| e.get())
}

pub struct Executor {
    pub vprocs: HashMap<VPid, Coroutine>,
    ready_queue: VecDeque<VPid>,
    next_pid: VPid,
    pub current: Option<VPid>,
    switch_count: u64,
    pub children: HashMap<VPid, Vec<VPid>>,
    pub saved_fork_lr: Option<u64>,
    exit_codes: HashMap<VPid, i32>,
}

impl Executor {
    pub fn new() -> Self {
        Executor {
            vprocs: HashMap::new(),
            ready_queue: VecDeque::new(),
            next_pid: 1,
            current: None,
            switch_count: 0,
            children: HashMap::new(),
            saved_fork_lr: None,
            exit_codes: HashMap::new(),
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

    pub fn spawn_front(&mut self, f: Box<dyn FnOnce()>) -> VPid {
        let pid = self.next_pid;
        self.next_pid += 1;
        let co = Coroutine::new(pid, f);
        self.vprocs.insert(pid, co);
        self.ready_queue.push_front(pid);
        pid
    }

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

    pub fn register_c_strings(&mut self, vpid: VPid, strings: Vec<*mut u8>) {
        if let Some(co) = self.vprocs.get_mut(&vpid) {
            co.c_strings = strings;
        }
    }

    pub fn register_mapped_region(&mut self, vpid: VPid, base: usize, size: usize) {
        if let Some(co) = self.vprocs.get_mut(&vpid) {
            co.mapped_regions.push((base, size));
        }
    }

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

    pub fn current_pid(&self) -> Option<VPid> {
        self.current
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

    fn deliver_signals(&mut self, pid: VPid) -> bool {
        let signals = {
            let co = match self.vprocs.get_mut(&pid) {
                Some(c) => c,
                None => return false,
            };
            if co.pending_signals.is_empty() {
                return false;
            }
            std::mem::take(&mut co.pending_signals)
        };
        for sig in &signals {
            match *sig {
                libc::SIGKILL | libc::SIGTERM => {
                    if let Some(co) = self.vprocs.get_mut(&pid) {
                        co.set_done(128 + sig);
                    }
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Run one scheduler step: resume the next ready coroutine.
    /// Returns true if work was done, false if nothing to run.
    fn step(&mut self) -> bool {
        let next_pid = match self.pick_next() {
            Some(pid) => pid,
            None => return false,
        };

        if self.deliver_signals(next_pid) {
            if let Some(cur) = self.current {
                self.ready_queue.push_back(cur);
            }
            return true;
        }

        // Re-queue current if still alive
        if let Some(cur) = self.current {
            if let Some(co) = self.vprocs.get(&cur) {
                if !co.is_done() {
                    self.ready_queue.push_back(cur);
                }
            }
        }

        self.current = Some(next_pid);
        self.switch_count += 1;
        self.vprocs.get_mut(&next_pid).unwrap().resume();
        true
    }

    pub fn r#yield(&mut self) {
        if let Some(pid) = self.current {
            let co = self.vprocs.get(&pid).unwrap();
            if !co.is_done() {
                self.vprocs.get_mut(&pid).unwrap().state = State::Ready;
                self.ready_queue.push_back(pid);
            }
        }
        self.step();
    }

    pub fn reap_done_coroutines(&mut self) {
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
        self.vprocs.retain(|&pid, co| {
            if co.is_done() {
                let real_fds = crate::vfd::remove_table_and_get_fds(pid);
                for fd in real_fds {
                    unsafe { crate::preload::real_close(fd); }
                }
                false
            } else {
                true
            }
        });
        // Don't release binary cache here — dlclose triggers __libc_init
        // which resets minicoro's thread-local _mco_main_ctx, breaking
        // subsequent coroutine context switching. Binary cache is managed
        // explicitly via unload_binary/unload_all_binaries.
    }

    pub fn block_on_all(&mut self) {
        set_global_executor(self as *mut Executor);
        while self.vprocs.values().any(|c| !c.is_done()) {
            let next_pid = match self.pick_next() {
                Some(pid) => pid,
                None => break,
            };
            self.deliver_signals(next_pid);
            self.current = Some(next_pid);
            self.switch_count += 1;
            self.vprocs.get_mut(&next_pid).unwrap().resume();
            if let Some(co) = self.vprocs.get(&next_pid) {
                if !co.is_done() {
                    self.ready_queue.push_back(next_pid);
                }
            }
        }
        self.reap_done_coroutines();
    }

    pub fn switch_count(&self) -> u64 {
        self.switch_count
    }
}

#[no_mangle]
pub extern "C" fn vproc_exit() {
    vproc_exit_with_code(0);
}

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
    ex.exit_codes.insert(pid, code);
    unsafe {
        let co = mco_running_raw();
        if !co.is_null() {
            mco_yield_raw(co);
        }
    }
}

pub fn get_exit_code(pid: VPid) -> Option<i32> {
    let ex = unsafe { &mut *get_global_executor() };
    if let Some(&code) = ex.exit_codes.get(&pid) {
        return Some(code);
    }
    ex.vprocs.get(&pid).and_then(|co| {
        if co.is_done() { Some(co.exit_code) } else { None }
    })
}

pub fn is_child_of(parent: VPid, child: VPid) -> bool {
    let ex = unsafe { &mut *get_global_executor() };
    ex.children.get(&parent).map_or(false, |kids| kids.contains(&child))
}

pub fn reap_child(parent: VPid, child: VPid) {
    let ex = unsafe { &mut *get_global_executor() };
    if let Some(kids) = ex.children.get_mut(&parent) {
        kids.retain(|&k| k != child);
    }
}

pub fn remove_exit_code(pid: VPid) {
    let ex = unsafe { &mut *get_global_executor() };
    ex.exit_codes.remove(&pid);
}

pub fn do_yield() {
    unsafe {
        let co = mco_running_raw();
        if !co.is_null() {
            // Re-queue current coroutine before yielding, otherwise the scheduler
            // never picks it up again (the same logic as Executor::r#yield).
            let ex = &mut *get_global_executor();
            if let Some(pid) = ex.current {
                if let Some(co_inner) = ex.vprocs.get(&pid) {
                    if !co_inner.is_done() {
                        ex.ready_queue.push_back(pid);
                    }
                }
            }
            mco_yield_raw(co);
        } else {
            // Driver thread — advance the scheduler.
            let ex = &mut *get_global_executor();
            ex.step_from_driver();
        }
    }
}

unsafe fn mco_running_raw() -> *mut crate::coroutine::McoCoro {
    unsafe extern "C" {
        fn mco_running() -> *mut crate::coroutine::McoCoro;
    }
    mco_running()
}

unsafe fn mco_yield_raw(co: *mut crate::coroutine::McoCoro) {
    unsafe extern "C" {
        fn mco_yield(co: *mut crate::coroutine::McoCoro) -> i32;
    }
    mco_yield(co);
}

/// Drive the scheduler from the driver thread (not from inside a coroutine).
impl Executor {
    pub fn step_from_driver(&mut self) {
        let next_pid = match self.pick_next() {
            Some(pid) => pid,
            None => return,
        };
        self.deliver_signals(next_pid);
        self.current = Some(next_pid);
        self.switch_count += 1;
        self.vprocs.get_mut(&next_pid).unwrap().resume();
        self.current = None;
    }

    /// Drive the scheduler with fd 0/1/2 swapped around the coroutine resume.
    /// Before resuming, installs the coroutine's real fds so fork children
    /// inherit the correct ones. After resume, restores the driver's originals.
    pub fn step_from_driver_with_fd_swap(&mut self, saved_fds: [std::os::raw::c_int; 3]) {
        let next_pid = match self.pick_next() {
            Some(pid) => pid,
            None => return,
        };
        self.deliver_signals(next_pid);
        let fds_swapped = if let Some(real_fds) = crate::vfd::get_real_fds(next_pid) {
            if unsafe { crate::ffi::raw_dup3(real_fds[0], 0) } < 0 { std::process::abort(); }
            if unsafe { crate::ffi::raw_dup3(real_fds[1], 1) } < 0 { std::process::abort(); }
            if unsafe { crate::ffi::raw_dup3(real_fds[2], 2) } < 0 { std::process::abort(); }
            true
        } else {
            false
        };
        self.current = Some(next_pid);
        self.switch_count += 1;
        self.vprocs.get_mut(&next_pid).unwrap().resume();
        self.current = None;
        if fds_swapped {
            if unsafe { crate::ffi::raw_dup3(saved_fds[0], 0) } < 0 { std::process::abort(); }
            if unsafe { crate::ffi::raw_dup3(saved_fds[1], 1) } < 0 { std::process::abort(); }
            if unsafe { crate::ffi::raw_dup3(saved_fds[2], 2) } < 0 { std::process::abort(); }
        }
    }
}
