//! Virtual file descriptor table for vproc.
//!
//! Each virtual process has its own fd namespace. Real fds (0,1,2) pass through
//! to the kernel. Virtual pipes are backed by in-process ring buffers.

use std::collections::HashMap;

const PIPE_CAPACITY: usize = 65536; // 64 KiB pipe buffer

/// A virtual file descriptor.
pub enum Vfd {
    /// Pass-through to a real kernel fd.
    Real(i32),
    /// Reading end of a virtual pipe.
    PipeRead(*mut PipeBuffer),
    /// Writing end of a virtual pipe.
    PipeWrite(*mut PipeBuffer),
}

// Safety: PipeBuffer is only accessed through &mut within a single coroutine
// context (cooperative scheduling means no concurrent access).
unsafe impl Send for Vfd {}
unsafe impl Sync for Vfd {}

/// Shared pipe buffer (ring buffer).
pub struct PipeBuffer {
    buf: Vec<u8>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
    closed: bool,
}

impl PipeBuffer {
    fn new() -> Self {
        PipeBuffer {
            buf: vec![0; PIPE_CAPACITY],
            read_pos: 0,
            write_pos: 0,
            len: 0,
            closed: false,
        }
    }

    pub fn read_from(&mut self, dst: &mut [u8]) -> isize {
        if self.len == 0 {
            if self.closed {
                return 0; // EOF
            }
            return -1; // EAGAIN
        }
        let n = dst.len().min(self.len);
        for i in 0..n {
            dst[i] = self.buf[self.read_pos];
            self.read_pos = (self.read_pos + 1) % PIPE_CAPACITY;
        }
        self.len -= n;
        n as isize
    }

    pub fn write_to(&mut self, src: &[u8]) -> isize {
        if self.closed {
            return -1; // EPIPE
        }
        let available = PIPE_CAPACITY - self.len;
        if available == 0 {
            return -1; // EAGAIN
        }
        let n = src.len().min(available);
        for i in 0..n {
            self.buf[self.write_pos] = src[i];
            self.write_pos = (self.write_pos + 1) % PIPE_CAPACITY;
        }
        self.len += n;
        n as isize
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len == PIPE_CAPACITY
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
        // Inherit standard fds from the host process
        table.fds.insert(0, Vfd::Real(0));
        table.fds.insert(1, Vfd::Real(1));
        table.fds.insert(2, Vfd::Real(2));
        table
    }

    fn alloc_fd(&mut self) -> u32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        // Skip over any already-used fds
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
            Some(Vfd::PipeRead(buf)) | Some(Vfd::PipeWrite(buf)) => {
                // Only close the underlying buffer if no other fd in this table
                // points to the same buffer (dup2 creates shared references).
                let buf_ptr = buf as usize;
                let still_referenced = self.fds.values().any(|vfd| match vfd {
                    Vfd::PipeRead(b) | Vfd::PipeWrite(b) => *b as usize == buf_ptr,
                    _ => false,
                });
                if !still_referenced {
                    unsafe { (*buf).close() };
                }
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
        // Close new_fd if it's already open
        if self.fds.contains_key(&new_fd) {
            let _ = self.close(new_fd);
        }
        match self.fds.get(&old_fd) {
            Some(vfd) => {
                let clone = match vfd {
                    Vfd::Real(fd) => Vfd::Real(*fd),
                    Vfd::PipeRead(buf) => Vfd::PipeRead(*buf),
                    Vfd::PipeWrite(buf) => Vfd::PipeWrite(*buf),
                };
                self.fds.insert(new_fd, clone);
                Ok(new_fd)
            }
            None => Err(9), // EBADF
        }
    }

    /// Create a virtual pipe. Returns (read_fd, write_fd).
    pub fn create_pipe(&mut self) -> (u32, u32) {
        let buf = Box::into_raw(Box::new(PipeBuffer::new()));
        let read_fd = self.alloc_fd();
        let write_fd = self.alloc_fd();
        self.fds.insert(read_fd, Vfd::PipeRead(buf));
        self.fds.insert(write_fd, Vfd::PipeWrite(buf));
        (read_fd, write_fd)
    }

    /// Create a shallow copy of this fd table for fork().
    ///
    /// Real fds: copied as-is (share kernel file descriptions).
    /// Pipe fds: share the same PipeBuffer pointer (matching Linux pipe semantics).
    ///
    /// # Safety: shared PipeBuffer lifetime
    ///
    /// Both parent and child hold raw pointers to the same PipeBuffer.
    /// When either VfdTable is dropped, it frees the PipeBuffer via
    /// Box::from_raw. This is safe only because cooperative scheduling
    /// guarantees no concurrent access, and for fork-then-execve the child
    /// gets a fresh fd table from execve. For long-lived parent/child pairs,
    /// reference counting would be needed.
    pub fn clone_for_fork(&self) -> Self {
        let mut new_table = VfdTable {
            fds: HashMap::new(),
            next_fd: self.next_fd,
        };
        for (&fd_num, vfd) in &self.fds {
            let cloned = match vfd {
                Vfd::Real(r) => Vfd::Real(*r),
                Vfd::PipeRead(buf) => Vfd::PipeRead(*buf),
                Vfd::PipeWrite(buf) => Vfd::PipeWrite(*buf),
            };
            new_table.fds.insert(fd_num, cloned);
        }
        new_table
    }
}

impl Drop for VfdTable {
    fn drop(&mut self) {
        // Don't free PipeBuffers here — they're shared across fork'd tables.
        // The PipeBuffer is cleaned up when close() is called on all references,
        // or leaked if the table is dropped without proper cleanup.
        // TODO: use Arc<PipeBuffer> for proper reference counting.
    }
}

// Global fd table storage, keyed by VPid.
// Uses AtomicPtr (like EXECUTOR_PTR) to survive TLS reinitialization
// when __libc_init runs inside a dlopen'd binary.
use std::collections::HashMap as StdHashMap;
use std::sync::atomic::{AtomicPtr, Ordering};

static FD_TABLES_PTR: AtomicPtr<StdHashMap<u32, VfdTable>> = AtomicPtr::new(std::ptr::null_mut());

/// Get or initialize the global fd tables map.
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

/// Get the fd table for the given virtual process.
pub fn get_table(vpid: u32) -> Option<&'static mut VfdTable> {
    unsafe { (*get_tables_ptr()).get_mut(&vpid) }
}

/// Get or create the fd table for a virtual process.
pub fn get_or_create_table(vpid: u32) -> &'static mut VfdTable {
    let tables = unsafe { &mut *get_tables_ptr() };
    tables.entry(vpid).or_insert_with(VfdTable::new)
}

/// Clone the fd table of parent_vpid for child_vpid (used by virtual fork).
pub fn fork_fd_table(parent_vpid: u32, child_vpid: u32) {
    let tables = unsafe { &mut *get_tables_ptr() };
    let child_table = match tables.get(&parent_vpid) {
        Some(parent_table) => parent_table.clone_for_fork(),
        None => VfdTable::new(),
    };
    tables.insert(child_vpid, child_table);
}
