//! Test: sh -c "echo hello" inside the vproc runtime.
//!
//! cargo run --example sh_test --release

fn main() {
    std::env::set_var("VPROC", "1");

    let shell = "/data/data/com.termux/files/usr/bin/sh";
    let argv = vec![
        shell.to_string(),
        "-c".to_string(),
        "echo hello".to_string(),
    ];
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

    println!("=== sh -c \"echo hello\" test ===");

    match vproc::vexec::virtual_execve_via_entry(shell, argv, envp) {
        Ok(exec) => {
            loop {
                match vproc::get_exit_code(exec.vpid) {
                    Some(code) => {
                        println!("[parent] shell exited with {}", code);
                        break;
                    }
                    None => {
                        vproc::r#yield();
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("[parent] failed: {}", e);
        }
    }

    println!("=== done ===");
    println!("switches: {}", vproc::switch_count());
}
