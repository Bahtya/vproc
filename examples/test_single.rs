//! Test harness: takes one or more shell commands as arguments, runs each
//! sequentially via virtual_execve_via_entry.

use vproc::executor::Executor;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmds: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(|s| s.as_str()).collect()
    } else {
        vec!["echo hello | cat"]
    };

    std::env::set_var("VPROC", "1");
    vproc::preload::install_crash_handler();

    let mut executor = Executor::new();
    vproc::executor::set_current_executor(&mut executor as *mut _);

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

    for cmd in &cmds {
        let argv = vec![shell.to_string(), "-c".to_string(), cmd.to_string()];
        match vproc::vexec::virtual_execve_via_entry(shell, argv, envp.clone()) {
            Ok(exec) => {
                // Drive the scheduler using step_from_driver to properly
                // clear executor.current after each resume (avoids PAC SIGSEGV
                // when the coroutine exits and yields back).
                loop {
                    executor.step_from_driver();
                    if vproc::get_exit_code(exec.vpid).is_some() {
                        break;
                    }
                    // Collect and process I/O waits
                    let (pollfds, pid_map) = vproc::executor::collect_io_waits();
                    if !pollfds.is_empty() {
                        unsafe {
                            libc::poll(
                                pollfds.as_ptr() as *mut libc::pollfd,
                                pollfds.len() as libc::nfds_t,
                                50,
                            )
                        };
                        vproc::executor::wake_io_ready(&pollfds, &pid_map);
                    }
                }
                executor.reap_done_coroutines();
            }
            Err(e) => eprintln!("FAILED: {}", e),
        }
    }

    vproc::cleanup_fd_tables();
}
