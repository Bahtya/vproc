//! Demo: virtual fork — create child coroutines without real processes.
//!
//! VPROC=1 cargo run --example fork_exec_demo --release

fn main() {
    std::env::set_var("VPROC", "1");

    println!("=== Virtual Fork Demo ===\n");

    vproc::spawn(Box::new(|| {
        // Test 1: fork + _exit + waitpid
        println!("--- Test 1: fork -> _exit(42) -> waitpid ---");
        {
            let pid = vproc_fork();
            if pid == 0 {
                println!("  [child] exiting with 42");
                std::process::exit(42);
            }
            println!("  [parent] child vpid = {}", pid);
            wait_child("1", pid, 42);
        }
        println!();

        // Test 2: multiple children (sequential)
        println!("--- Test 2: 3 sequential fork children ---");
        {
            let pid_a = vproc_fork();
            if pid_a == 0 {
                println!("  [child A] exiting with 10");
                vproc::executor::vproc_exit_with_code(10);
            }
            println!("  [parent] child A vpid = {}", pid_a);

            let pid_b = vproc_fork();
            if pid_b == 0 {
                println!("  [child B] exiting with 20");
                vproc::executor::vproc_exit_with_code(20);
            }
            println!("  [parent] child B vpid = {}", pid_b);

            let pid_c = vproc_fork();
            if pid_c == 0 {
                println!("  [child C] exiting with 30");
                vproc::executor::vproc_exit_with_code(30);
            }
            println!("  [parent] child C vpid = {}", pid_c);

            wait_child("A", pid_a, 10);
            wait_child("B", pid_b, 20);
            wait_child("C", pid_c, 30);
        }

        println!("\n=== All tests done ===");
        println!("switches: {}", vproc::switch_count());
    }));

    vproc::block_on_all();
}

fn wait_child(name: &str, pid: u32, expected: i32) {
    let mut tries = 0;
    loop {
        tries += 1;
        match vproc::get_exit_code(pid) {
            Some(code) => {
                println!("  [parent] child {} exited with {} (expected {}) {}",
                    name, code, expected, if code == expected { "OK" } else { "FAIL" });
                return;
            }
            None => {
                if tries <= 2 {
                    eprintln!("  [debug] wait_child({}): pid {} not done yet, yielding...", name, pid);
                }
                vproc::r#yield();
            }
        }
    }
}

/// Virtual fork using the helper coroutine pattern.
fn vproc_fork() -> u32 {
    // Check if we are a fork child resuming after do_yield
    let fork_result = vproc::executor::EXECUTOR.with(|e| unsafe {
        let ex = &mut *e.get();
        let pid = ex.current.unwrap();
        let co = ex.vprocs.get(&pid).unwrap();
        if co.is_fork_child {
            ex.vprocs.get_mut(&pid).unwrap().is_fork_child = false;
            return 0u32;
        }
        u32::MAX
    });

    if fork_result != u32::MAX {
        return fork_result;
    }

    vproc::spawn(Box::new(move || {
        vproc::executor::EXECUTOR.with(|e| unsafe {
            (&mut *e.get()).spawn_fork_child();
        });
    }));

    vproc::executor::do_yield();

    vproc::executor::EXECUTOR.with(|e| unsafe {
        let ex = &mut *e.get();
        let pid = ex.current.unwrap();
        ex.vprocs.get(&pid).unwrap().fork_child_pid
    })
}
