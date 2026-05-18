//! Virtual file descriptor table for vproc.
//!
//! Each virtual process has its own fd namespace. Real fds (0,1,2) pass through
//! to the kernel. Virtual pipes are backed by in-process ring buffers shared
//! via Arc reference counting.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::Arc;

const PIPE_CAPACITY: usize = 65536; // 64 KiB pipe buffer

/// A virtual file descriptor.
pub enum Vfd {
    /// Pass-through to a real kernel fd.
    Real(i32),
    /// Reading end of a virtual pipe.
    PipeRead(Arc<PipeBuffer>),
    /// Writing end of a virtual pipe.
    PipeWrite(Arc<PipeBuffer>),
}

/// Shared pipe buffer (ring buffer) with interior mutability.
///
/// Safe under cooperative scheduling: only one coroutine executes at a time,
/// so mutable access through `UnsafeCell` cannot race.
pub struct PipeBuffer {
    inner: UnsafeCell<PipeBufferInner>,
}

struct PipeBufferInner {
    buf: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
    closed: bool,
}

// Safe: cooperative scheduling guarantees no concurrent access.
unsafe impl Send for PipeBuffer {}
unsafe impl Sync for PipeBuffer {}

impl PipeBuffer {
    fn new() -> Self {
        PipeBuffer {
            inner: UnsafeCell::new(PipeBufferInner {
                buf: vec![0; PIPE_CAPACITY],
                read_pos: 0,
                write_pos: 0,
                len: 0,
                closed: false,
            }),
        }
    }

    fn inner(&self) -> &mut PipeBufferInner {
        unsafe { &mut *self.inner.get() }
    }

    pub fn read_from(&self, dst: &mut [u8]) -> isize {
        let inner = self.inner();
        if inner.len == 0 {
            if inner.closed {
                return 0; // EOF
            }
            return -1; // EAGAIN
        }
        let n = dst.len().min(inner.len);
        for i in 0..n {
            dst[i] = inner.buf[inner.read_pos];
            inner.read_pos = (inner.read_pos + 1) % PIPE_CAPACITY;
        }
        inner.len -= n;
        n as isize
    }

    pub fn write_to(&self, src: &[u8]) -> isize {
        let inner = self.inner();
        if inner.closed {
            return -1; // EPIPE
        }
        let available = PIPE_CAPACITY - inner.len;
        if available == 0 {
            return -1; // EAGAIN
        }
        let n = src.len().min(available);
        for i in 0..n {
            inner.buf[inner.write_pos] = src[i];
            inner.write_pos = (inner.write_pos + 1) % PIPE_CAPACITY;
        }
        inner.len += n;
        n as isize
    }

    pub fn close(&self) {
        self.inner().closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.inner().closed
    }

    pub fn is_empty(&self) -> bool {
        self.inner().len == 0
    }

    pub fn is_full(&self) -> bool {
        self.inner().len == PIPE_CAPACITY
    }
}

/// Virtual fd table for one virtual process.
pub struct VfdTable {
    fds: HashMap<u32, Vfd>,
    next_fd: u32,
}

impl VfdTable {
    pub fn new() -> Self {
        let mut table = VfdTable {
            fds: HashMap::new(),
            next_fd: 3, // 0=stdin, 1=stdout, 2=stderr reserved
        };
        table.fds.insert(0, Vfd::Real(0));
        table.fds.insert(1, Vfd::Real(1));
        table.fds.insert(2, Vfd::Real(2));
        table
    }

    fn alloc_fd(&mut self) -> u32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        while self.fds.contains_key(&self.next_fd) {
            self.next_fd += 1;
        }
        fd
    }

    pub fn insert(&mut self, vfd: Vfd) -> u32 {
        let fd = self.alloc_fd();
        self.fds.insert(fd, vfd);
        fd
    }

    pub fn get(&self, fd: u32) -> Option<&Vfd> {
        self.fds.get(&fd)
    }

    pub fn get_mut(&mut self, fd: u32) -> Option<&mut Vfd> {
        self.fds.get_mut(&fd)
    }

    pub fn close(&mut self, fd: u32) -> Result<(), i32> {
        match self.fds.remove(&fd) {
            Some(Vfd::PipeRead(arc)) | Some(Vfd::PipeWrite(arc)) => {
                // Close the buffer when no fd in this table still references it.
                let still_referenced = self.fds.values().any(|vfd| match vfd {
                    Vfd::PipeRead(a) | Vfd::PipeWrite(a) => Arc::ptr_eq(a, &arc),
                    _ => false,
                });
                if !still_referenced {
                    arc.close();
                }
                // Arc drops here — if ref count reaches zero, PipeBuffer is freed.
                Ok(())
            }
            Some(_) => Ok(()),
            None => Err(9), // EBADF
        }
    }

    pub fn dup(&mut self, old_fd: u32) -> Result<u32, i32> {
        let new_fd = self.alloc_fd();
        self.dup2(old_fd, new_fd)
    }

    pub fn dup2(&mut self, old_fd: u32, new_fd: u32) -> Result<u32, i32> {
        if self.fds.contains_key(&new_fd) {
            let _ = self.close(new_fd);
        }
        match self.fds.get(&old_fd) {
            Some(vfd) => {
                let clone = match vfd {
                    Vfd::Real(fd) => Vfd::Real(*fd),
                    Vfd::PipeRead(arc) => Vfd::PipeRead(Arc::clone(arc)),
                    Vfd::PipeWrite(arc) => Vfd::PipeWrite(Arc::clone(arc)),
                };
                self.fds.insert(new_fd, clone);
                Ok(new_fd)
            }
            None => Err(9), // EBADF
        }
    }

    /// Create a virtual pipe. Returns (read_fd, write_fd).
    pub fn create_pipe(&mut self) -> (u32, u32) {
        let buf = Arc::new(PipeBuffer::new());
        let read_fd = self.alloc_fd();
        let write_fd = self.alloc_fd();
        self.fds.insert(read_fd, Vfd::PipeRead(Arc::clone(&buf)));
        self.fds.insert(write_fd, Vfd::PipeWrite(buf));
        (read_fd, write_fd)
    }

    /// Clone fd table for fork(). Arc reference counts are incremented,
    /// so PipeBuffer is freed only when all tables drop their references.
    pub fn clone_for_fork(&self) -> Self {
        let mut new_table = VfdTable {
            fds: HashMap::new(),
            next_fd: self.next_fd,
        };
        for (&fd_num, vfd) in &self.fds {
            let cloned = match vfd {
                Vfd::Real(r) => Vfd::Real(*r),
                Vfd::PipeRead(arc) => Vfd::PipeRead(Arc::clone(arc)),
                Vfd::PipeWrite(arc) => Vfd::PipeWrite(Arc::clone(arc)),
            };
            new_table.fds.insert(fd_num, cloned);
        }
        new_table
    }
}

impl Drop for VfdTable {
    fn drop(&mut self) {
        // Mark all pipe buffers as closed so any waiting coroutine can detect EOF.
        // Arc handles deallocation when the last reference is dropped.
        for vfd in self.fds.values() {
            match vfd {
                Vfd::PipeRead(arc) | Vfd::PipeWrite(arc) => {
                    arc.close();
                }
                _ => {}
            }
        }
    }
}

// Global fd table storage, keyed by VPid.
use std::collections::HashMap as StdHashMap;
use std::sync::atomic::{AtomicPtr, Ordering};

static FD_TABLES_PTR: AtomicPtr<StdHashMap<u32, VfdTable>> = AtomicPtr::new(std::ptr::null_mut());

fn get_tables_ptr() -> *mut StdHashMap<u32, VfdTable> {
    let ptr = FD_TABLES_PTR.load(Ordering::SeqCst);
    if !ptr.is_null() {
        return ptr;
    }
    let new = Box::into_raw(Box::new(StdHashMap::new()));
    match FD_TABLES_PTR.compare_exchange(
        std::ptr::null_mut(),
        new,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => new,
        Err(existing) => {
            unsafe { drop(Box::from_raw(new)); }
            existing
        }
    }
}

pub fn get_table(vpid: u32) -> Option<&'static mut VfdTable> {
    unsafe { (*get_tables_ptr()).get_mut(&vpid) }
}

pub fn get_or_create_table(vpid: u32) -> &'static mut VfdTable {
    let tables = unsafe { &mut *get_tables_ptr() };
    tables.entry(vpid).or_insert_with(VfdTable::new)
}

pub fn fork_fd_table(parent_vpid: u32, child_vpid: u32) {
    let tables = unsafe { &mut *get_tables_ptr() };
    let child_table = match tables.get(&parent_vpid) {
        Some(parent_table) => parent_table.clone_for_fork(),
        None => VfdTable::new(),
    };
    tables.insert(child_vpid, child_table);
}

/// Remove a virtual process's fd table. Called when the coroutine exits.
pub fn remove_table(vpid: u32) {
    let ptr = get_tables_ptr();
    if !ptr.is_null() {
        unsafe { (*ptr).remove(&vpid); }
    }
}

/// Clean up the global fd tables map. Called during shutdown.
pub fn cleanup() {
    let ptr = FD_TABLES_PTR.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !ptr.is_null() {
        unsafe { drop(Box::from_raw(ptr)); }
    }
}
