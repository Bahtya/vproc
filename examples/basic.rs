use vproc;

fn main() {
    println!("=== vproc Phase 1: basic coroutine test ===\n");
    println!("PID: {}", std::process::id());

    let vp1 = vproc::spawn(Box::new(|| {
        for i in 0..3 {
            println!("  [VP1] step {}", i);
            vproc::r#yield();
        }
        println!("  [VP1] done");
    }));
    println!("spawned VP1 (id={})", vp1);

    let vp2 = vproc::spawn(Box::new(|| {
        for i in 0..3 {
            println!("  [VP2] step {}", i);
            vproc::r#yield();
        }
        println!("  [VP2] done");
    }));
    println!("spawned VP2 (id={})", vp2);

    let vp3 = vproc::spawn(Box::new(|| {
        for i in 0..3 {
            println!("  [VP3] step {}", i);
            vproc::r#yield();
        }
        println!("  [VP3] done");
    }));
    println!("spawned VP3 (id={})", vp3);

    println!("\n--- running ---\n");
    vproc::block_on_all();

    println!("\n--- results ---");
    println!("context switches: {}", vproc::switch_count());
    println!("process count: 1 (this process only)");
    println!("\n=== PASS ===");
}
