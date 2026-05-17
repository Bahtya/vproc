//! E2E test: C LD_PRELOAD layer intercepts exit() inside coroutines.
//!
//! Build: make e2e  (from preload/)
//! This compiles with -rdynamic so the C preload can find vproc_ffi_* symbols.
//!
//! Expected: process survives, exit codes captured correctly.

fn main() {
    println!("=== E2E Test: C Preload + vproc Runtime ===\n");

    // Test 1: single coroutine calls exit(42)
    println!("--- Test 1: exit(42) interception ---");
    let child = vproc::spawn(Box::new(|| {
        print!("  [child] calling libc::exit(42)\n");
        unsafe { libc::exit(42) };
    }));
    loop {
        match vproc::get_exit_code(child) {
            Some(code) => {
                assert_eq!(code, 42, "expected exit code 42, got {}", code);
                println!("  [parent] child exited with {} OK", code);
                break;
            }
            None => vproc::r#yield(),
        }
    }
    println!("  Test 1 PASSED\n");

    // Test 2: two coroutines exit with different codes
    println!("--- Test 2: multiple coroutines ---");
    let c1 = vproc::spawn(Box::new(|| unsafe { libc::exit(10) }));
    let c2 = vproc::spawn(Box::new(|| unsafe { libc::exit(20) }));
    let mut got1 = false;
    let mut got2 = false;
    loop {
        if !got1 {
            if let Some(code) = vproc::get_exit_code(c1) {
                assert_eq!(code, 10);
                println!("  c1 exited with {} OK", code);
                got1 = true;
            }
        }
        if !got2 {
            if let Some(code) = vproc::get_exit_code(c2) {
                assert_eq!(code, 20);
                println!("  c2 exited with {} OK", code);
                got2 = true;
            }
        }
        if got1 && got2 {
            break;
        }
        vproc::r#yield();
    }
    println!("  Test 2 PASSED\n");

    // Test 3: _exit() also intercepted
    println!("--- Test 3: _exit(99) interception ---");
    let child = vproc::spawn(Box::new(|| unsafe {
        libc::_exit(99);
    }));
    loop {
        match vproc::get_exit_code(child) {
            Some(code) => {
                assert_eq!(code, 99);
                println!("  child _exit(99) → got {} OK", code);
                break;
            }
            None => vproc::r#yield(),
        }
    }
    println!("  Test 3 PASSED\n");

    println!("=== E2E PASS ===");
    println!("switches: {}", vproc::switch_count());
}
