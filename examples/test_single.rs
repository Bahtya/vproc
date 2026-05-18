//! Test harness: takes one or more shell commands as arguments, runs each
//! sequentially via virtual_execve_via_entry.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmds: Vec<&str> = if args.len() > 1 {
        args[1..].iter().map(|s| s.as_str()).collect()
    } else {
        vec!["echo hello | cat"]
    };

    std::env::set_var("VPROC", "1");
    vproc::preload::install_crash_handler();

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

    for cmd in &cmds {
        let argv = vec![shell.to_string(), "-c".to_string(), cmd.to_string()];
        match vproc::vexec::virtual_execve_via_entry(shell, argv, envp.clone()) {
            Ok(exec) => {
                let mut y = 0;
                loop {
                    match vproc::get_exit_code(exec.vpid) {
                        Some(_) => { break; }
                        None => { vproc::r#yield(); y += 1; if y > 5000 { break; } }
                    }
                }
            }
            Err(e) => eprintln!("FAILED: {}", e),
        }
    }
}
