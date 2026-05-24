//! Debug test: use pipe instead of pty, check if shell output goes to the right fd.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::time::Instant;

fn raw_write(fd: c_int, buf: &[u8]) -> isize {
    unsafe { libc::syscall(64, fd, buf.as_ptr() as *const c_void, buf.len()) as isize }
}

fn raw_read(fd: c_int, buf: &mut [u8]) -> isize {
    unsafe { libc::syscall(63, fd, buf.as_mut_ptr() as *mut c_void, buf.len()) as isize }
}

fn log(msg: &str) {
    let _ = raw_write(2, format!("{}\n", msg).as_bytes());
}

fn main() {
    std::env::set_var("VPROC", "1");
    vproc::preload::install_crash_handler();
    log("[test] starting");

    // Create pipes for stdin, stdout, stderr
    let mut stdin_pipe = [-1i32, -1i32];  // [read, write]
    let mut stdout_pipe = [-1i32, -1i32];
    let mut stderr_pipe = [-1i32, -1i32];
    unsafe {
        libc::pipe(stdin_pipe.as_mut_ptr());
        libc::pipe(stdout_pipe.as_mut_ptr());
        libc::pipe(stderr_pipe.as_mut_ptr());
    }
    log(&format!("[test] stdin pipe: r={} w={}", stdin_pipe[0], stdin_pipe[1]));
    log(&format!("[test] stdout pipe: r={} w={}", stdout_pipe[0], stdout_pipe[1]));
    log(&format!("[test] stderr pipe: r={} w={}", stderr_pipe[0], stderr_pipe[1]));

    // Close read end of stdin (shell won't read) and write ends of stdout/stderr (we read)
    unsafe {
        libc::close(stdin_pipe[0]);   // close read end — shell doesn't need stdin
        // DON'T close write ends of stdout/stderr yet — coroutine needs them
    }

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let shell_c = CString::new(shell).unwrap();
    let c1 = CString::new("sh").unwrap();
    let c2 = CString::new("-c").unwrap();
    let c3 = CString::new("echo hello_vproc").unwrap();
    let argv: [*const c_char; 4] = [c1.as_ptr(), c2.as_ptr(), c3.as_ptr(), std::ptr::null()];

    let envp: Vec<CString> = std::env::vars()
        .map(|(k, v)| CString::new(format!("{}={}", k, v)).unwrap())
        .collect();
    let envp_ptrs: Vec<*const c_char> = envp.iter().map(|cs| cs.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();

    // stdin=write_end, stdout=write_end, stderr=write_end
    let t0 = Instant::now();
    let session_id = vproc::ffi::vproc_ffi_create_session();
    log(&format!("[test] session_id={session_id}"));
    let vpid = unsafe { vproc::ffi::vproc_ffi_create_process(
        session_id,
        shell_c.as_ptr(), argv.as_ptr(), envp_ptrs.as_ptr(),
        stdin_pipe[1], stdout_pipe[1], stderr_pipe[1],
    ) };
    log(&format!("[test] create_process → vpid={vpid} ({:?})", t0.elapsed()));

    if vpid == 0 {
        log("[test] FAILED");
        std::process::exit(1);
    }

    // Wait for coroutine to finish
    log("[test] waiting for coroutine...");
    let exit_code = vproc::ffi::vproc_ffi_run_until_exit(session_id, vpid);
    log(&format!("[test] exit_code={exit_code} ({:?})", t0.elapsed()));

    // Read from stdout pipe
    let mut buf = [0u8; 4096];
    let n = raw_read(stdout_pipe[0], &mut buf);
    if n > 0 {
        let text = String::from_utf8_lossy(&buf[..n as usize]);
        log(&format!("[test] stdout: {}", text.trim()));
    } else {
        log("[test] stdout: (empty)");
    }

    // Read from stderr pipe
    let n = raw_read(stderr_pipe[0], &mut buf);
    if n > 0 {
        let text = String::from_utf8_lossy(&buf[..n as usize]);
        log(&format!("[test] stderr: {}", text.trim()));
    } else {
        log("[test] stderr: (empty)");
    }

    unsafe {
        libc::close(stdout_pipe[0]); libc::close(stdout_pipe[1]);
        libc::close(stderr_pipe[0]); libc::close(stderr_pipe[1]);
        libc::close(stdin_pipe[1]);
    }

    log("[test] done");
}
