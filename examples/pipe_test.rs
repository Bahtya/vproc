//! Test: sh -c pipe commands
fn main() {
    std::env::set_var("VPROC", "1");

    let tests: &[(&str, &str)] = &[
        ("echo hello | wc -w", "pipe: echo | wc"),
        ("echo hello | cat", "pipe: echo | cat"),
    ];

    for (cmd, label) in tests {
        println!("--- {} ---", label);
        let shell = "/data/data/com.termux/files/usr/bin/sh";
        let argv = vec![shell.to_string(), "-c".to_string(), cmd.to_string()];
        let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

        match vproc::vexec::virtual_execve_via_entry(shell, argv, envp) {
            Ok(exec) => {
                let mut yields = 0;
                loop {
                    match vproc::get_exit_code(exec.vpid) {
                        Some(code) => {
                            println!("[result] exit code: {}", code);
                            break;
                        }
                        None => {
                            vproc::r#yield();
                            yields += 1;
                            if yields > 10000 {
                                eprintln!("[STUCK] too many yields for '{}'", cmd);
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => eprintln!("[FAILED] {}", e),
        }
    }

    println!("=== pipe tests done ===");
}
