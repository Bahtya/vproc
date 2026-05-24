//! Test fork+exec in coroutine context
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

fn raw_write(fd: c_int, buf: &[u8]) -> isize {
    unsafe { libc::syscall(64, fd, buf.as_ptr() as *const _, buf.len()) as isize }
}

fn log(msg: &str) {
    let _ = raw_write(2, format!("{}\n", msg).as_bytes());
}

fn main() {
    std::env::set_var("VPROC", "1");
    vproc::preload::install_crash_handler();
    log("[test] starting fork+exec test");

    // Create session
    let session_id = vproc::ffi::vproc_ffi_create_session();
    log(&format!("[test] session_id={}", session_id));
    if session_id == 0 {
        log("[test] FAILED: create_session returned 0");
        std::process::exit(1);
    }

    // Run probe
    let probe = vproc::ffi::vproc_ffi_probe();
    log(&format!("[test] probe={}", probe));
    if probe < 0 {
        log("[test] FAILED: probe failed");
        std::process::exit(1);
    }

    // Create pty pair
    let mut master_fd: c_int = -1;
    let mut slave_fd: c_int = -1;
    let ret = unsafe { libc::openpty(&mut master_fd, &mut slave_fd, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
    if ret != 0 {
        log(&format!("[test] openpty failed: {}", ret));
        std::process::exit(1);
    }
    log(&format!("[test] pty: master={} slave={}", master_fd, slave_fd));

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let cmd = "echo pipe_test | cat";
    let shell_c = CString::new(shell).unwrap();
    let c1 = CString::new("sh").unwrap();
    let c2 = CString::new("-c").unwrap();
    let c3 = CString::new(cmd).unwrap();
    let argv: [*const c_char; 4] = [c1.as_ptr(), c2.as_ptr(), c3.as_ptr(), std::ptr::null()];

    let envp: Vec<CString> = std::env::vars()
        .map(|(k, v)| CString::new(format!("{}={}", k, v)).unwrap())
        .collect();
    let envp_ptrs: Vec<*const c_char> = envp.iter().map(|cs| cs.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();

    let vpid = unsafe { vproc::ffi::vproc_ffi_create_process(
        session_id,
        shell_c.as_ptr(), argv.as_ptr(), envp_ptrs.as_ptr(),
        slave_fd, slave_fd, slave_fd,
    ) };
    log(&format!("[test] vpid={}", vpid));

    if vpid == 0 {
        log("[test] FAILED: create_process returned 0");
        std::process::exit(1);
    }

    log("[test] calling run_until_exit...");
    let exit_code = vproc::ffi::vproc_ffi_run_until_exit(session_id, vpid);
    log(&format!("[test] exit_code={}", exit_code));

    // Read output from master
    let mut buf = [0u8; 4096];
    let n = raw_read(master_fd, &mut buf);
    if n > 0 {
        let text = String::from_utf8_lossy(&buf[..n as usize]);
        log(&format!("[test] output: {}", text.trim()));
    } else {
        log("[test] no output");
    }

    unsafe {
        libc::close(master_fd);
        libc::close(slave_fd);
    }
    log("[test] done");
}

fn raw_read(fd: c_int, buf: &mut [u8]) -> isize {
    unsafe { libc::syscall(63, fd, buf.as_mut_ptr() as *mut _, buf.len()) as isize }
}
