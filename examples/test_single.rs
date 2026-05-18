//! Single test: takes command as argument.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = if args.len() > 1 { &args[1] } else { "echo hello | /data/data/com.termux/files/usr/bin/cat" };

    std::env::set_var("VPROC", "1");
    vproc::preload::install_crash_handler();

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();
    let argv = vec![shell.to_string(), "-c".to_string(), cmd.to_string()];

    eprintln!("[test] cmd: '{}'", cmd);
    match vproc::vexec::virtual_execve_via_entry(shell, argv, envp) {
        Ok(exec) => {
            let mut y = 0;
            loop {
                match vproc::get_exit_code(exec.vpid) {
                    Some(code) => { eprintln!("[test] exit {}", code); break; }
                    None => { vproc::r#yield(); y += 1; if y > 5000 { eprintln!("[test] STUCK"); break; } }
                }
            }
        }
        Err(e) => eprintln!("[test] FAILED: {}", e),
    }
}
