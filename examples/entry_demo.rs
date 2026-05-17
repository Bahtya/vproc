//! Demo: load real Termux binaries via dlopen + entry point jump.
//!
//! VPROC=1 cargo run --example entry_demo --release

fn run_binary(path: &str, args: &[&str]) {
    let mut argv = vec![path.to_string()];
    for a in args {
        argv.push(a.to_string());
    }
    let envp: Vec<String> = std::env::vars().map(|(k, v)| format!("{}={}", k, v)).collect();

    println!("--- Loading: {} ---", path);
    match vproc::vexec::virtual_execve_via_entry(path, argv, envp) {
        Ok(exec) => {
            println!("[parent] spawned vpid={}", exec.vpid);
            loop {
                match vproc::get_exit_code(exec.vpid) {
                    Some(code) => {
                        println!("[parent] exited with {}", code);
                        break;
                    }
                    None => vproc::r#yield(),
                }
            }
        }
        Err(e) => {
            eprintln!("[parent] failed: {}", e);
        }
    }
}

fn main() {
    println!("=== Entry Point Demo: Real Termux Binaries ===\n");

    // Set VPROC=1 so our Rust preload intercepts exit()
    std::env::set_var("VPROC", "1");

    // Test 1: /usr/bin/true (simplest binary, should exit 0)
    run_binary("/data/data/com.termux/files/usr/bin/true", &[]);
    println!();

    // Test 2: /usr/bin/false (should exit 1)
    run_binary("/data/data/com.termux/files/usr/bin/false", &[]);
    println!();

    // Test 3: /usr/bin/echo
    run_binary("/data/data/com.termux/files/usr/bin/echo", &["hello", "from", "vproc"]);
    println!();

    println!("=== All tests done ===");
    println!("switches: {}", vproc::switch_count());
}
