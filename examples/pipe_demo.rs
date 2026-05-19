//! Phase 4 demo: exit code + pipes.

fn main() {
    println!("=== vproc Phase 4 demo ===\n");

    // --- Test 1: exit code propagation ---
    println!("--- Test 1: exit code propagation ---");
    vproc::spawn(Box::new(|| {
        let child = vproc::spawn(Box::new(|| {
            print!("  [child] about to exit(42)\n");
            vproc::exit(42);
            print!("  [child] AFTER exit (should not reach here)\n");
        }));
        print!("  [parent] spawned child={}\n", child);

        loop {
            let code = vproc::get_exit_code(child);
            print!("  [parent] get_exit_code({}) = {:?}\n", child, code);
            if code.is_some() {
                let c = code.unwrap();
                print!("  [parent] child exited with {} (expected 42)\n", c);
                assert_eq!(c, 42);
                break;
            }
            vproc::r#yield();
        }
    }));
    vproc::block_on_all();
    println!("  Test 1 PASSED\n");

    // --- Test 2: virtual pipe ---
    println!("--- Test 2: virtual pipe ---");
    vproc::spawn(Box::new(|| {
        let table = vproc::vfd::get_or_create_table(100);
        let (read_fd, write_fd) = table.create_pipe();
        println!("  pipe: read={}, write={}", read_fd, write_fd);

        let pipe_buf_w = match table.get(write_fd) {
            Some(vproc::vfd::Vfd::PipeWrite(buf)) => buf.clone(),
            _ => panic!("no write fd"),
        };
        let pipe_buf_r = match table.get(read_fd) {
            Some(vproc::vfd::Vfd::PipeRead(buf)) => buf.clone(),
            _ => panic!("no read fd"),
        };

        let writer = vproc::spawn(Box::new(move || {
            let msg = b"hello pipe!\n";
            loop {
                let n = pipe_buf_w.write_to(msg);
                if n > 0 { print!("  [writer] wrote {} bytes\n", n); break; }
                vproc::r#yield();
            }
        }));

        let reader = vproc::spawn(Box::new(move || {
            let mut buf = [0u8; 64];
            loop {
                let n = pipe_buf_r.read_from(&mut buf);
                if n > 0 {
                    let s = std::str::from_utf8(&buf[..n as usize]).unwrap();
                    print!("  [reader] got: {}", s);
                    break;
                }
                vproc::r#yield();
            }
        }));

        loop {
            let w = vproc::get_exit_code(writer).is_some();
            let r = vproc::get_exit_code(reader).is_some();
            if w && r { break; }
            vproc::r#yield();
        }
        println!("  Test 2 PASSED");
    }));
    vproc::block_on_all();

    println!("\n=== Phase 4 PASS ===");
    println!("switches: {}", vproc::switch_count());
}
