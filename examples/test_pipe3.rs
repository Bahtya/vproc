//! Minimal pipe test with direct crash diagnosis.
//! Writes debug output to fd 2 (stderr) using raw syscalls only.
fn main() {
    std::env::set_var("VPROC", "1");

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

    // Install a simpler crash handler first
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = simple_crash_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }

    raw_trace(b"[test] before execve\n");
    let argv = vec![shell.to_string(), "-c".to_string(), "echo hello | /data/data/com.termux/files/usr/bin/cat".to_string()];

    match vproc::vexec::virtual_execve_via_entry(shell, argv, envp) {
        Ok(exec) => {
            raw_trace(b"[test] spawned ok, yielding...\n");
            let mut y = 0;
            loop {
                match vproc::get_exit_code(exec.vpid) {
                    Some(code) => { raw_trace(format!("[test] exit {}\n", code).as_bytes()); break; }
                    None => {
                        if y == 0 { raw_trace(b"[test] first yield...\n"); }
                        vproc::r#yield();
                        if y == 0 { raw_trace(b"[test] first yield returned\n"); }
                        y += 1;
                        if y > 5000 { raw_trace(b"[test] STUCK\n"); break; }
                    }
                }
            }
        }
        Err(e) => { raw_trace(format!("[test] FAILED: {}\n", e).as_bytes()); }
    }
}

fn raw_trace(msg: &[u8]) {
    unsafe { libc::syscall(64, 2, msg.as_ptr() as *const _, msg.len()); }
}

extern "C" fn simple_crash_handler(
    sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    ctx: *mut std::os::raw::c_void,
) {
    // First: reset handler to prevent recursive crash
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }

    raw_trace(b"\n[CRASH] entered handler\n");

    let fault_addr = unsafe { (*_info).si_addr() as usize };
    raw_trace(format!("[CRASH] sig={} fault={:#x}\n", sig, fault_addr).as_bytes());

    // Read fault registers from ucontext
    // bionic aarch64: mcontext at ctx+176, sigcontext: regs[31]=8..256, sp=256, pc=264
    let fault_pc: usize;
    let saved_sp: usize;
    let saved_lr: usize;
    unsafe {
        let mctx = ctx.add(176);
        fault_pc = mctx.add(264).cast::<usize>().read();
        saved_sp = mctx.add(256).cast::<usize>().read();
        saved_lr = mctx.add(248).cast::<usize>().read();
    }

    let mut buf = [0u8; 512];
    let n = unsafe {
        libc::snprintf(
            buf.as_mut_ptr() as *mut libc::c_char, 512,
            b"[CRASH] pc=%p sp=%p lr=%p\n\0".as_ptr() as *const _,
            fault_pc as *const std::os::raw::c_void,
            saved_sp as *const std::os::raw::c_void,
            saved_lr as *const std::os::raw::c_void,
        )
    };
    if n > 0 { unsafe { libc::syscall(64, 2, buf.as_ptr() as *const _, n as usize); } }

    // Walk frame chain
    let mut fp = saved_sp;
    for depth in 0..10usize {
        if fp == 0 { break; }
        let next_fp: usize = unsafe { (fp as *const usize).read() };
        let ret_addr: usize = unsafe { ((fp + 8) as *const usize).read() };
        let mut _b3 = [0u8; 256];
        let _n3 = unsafe { libc::snprintf(
            _b3.as_mut_ptr() as *mut libc::c_char, 256,
            b"  #%d fp=%p lr=%p\n\0".as_ptr() as *const libc::c_char,
            depth, next_fp as *const std::os::raw::c_void,
            ret_addr as *const std::os::raw::c_void,
        ) };
        if _n3 > 0 { unsafe { libc::syscall(64, 2, _b3.as_ptr() as *const _, _n3 as usize); } }
        if next_fp == 0 || next_fp <= fp { break; }
        fp = next_fp;
    }

    unsafe { libc::_exit(128 + sig); }
}
