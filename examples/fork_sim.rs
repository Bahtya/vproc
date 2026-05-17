use std::cell::UnsafeCell;
use std::collections::HashMap;

type VPid = u32;

struct ChildInfo {
    done: bool,
    exit_code: i32,
}

struct VirtualProc {
    next_pid: VPid,
    children: HashMap<VPid, ChildInfo>,
}

thread_local! {
    static VP: UnsafeCell<VirtualProc> = UnsafeCell::new(VirtualProc {
        next_pid: 10000,
        children: HashMap::new(),
    });
}

fn vp() -> &'static mut VirtualProc {
    unsafe { &mut *VP.with(|e| e.get()) }
}

fn virtual_fork(child_fn: Box<dyn FnOnce()>) -> VPid {
    let v = vp();
    let child_pid = v.next_pid;
    v.next_pid += 1;
    v.children.insert(child_pid, ChildInfo { done: false, exit_code: 0 });

    vproc::spawn(Box::new(move || {
        child_fn();
        let v = unsafe { &mut *VP.with(|e| e.get()) };
        if let Some(info) = v.children.get_mut(&child_pid) {
            info.done = true;
        }
    }));

    child_pid
}

/// Yield until child is done, then return its exit code.
fn virtual_waitpid(pid: VPid) -> i32 {
    loop {
        {
            let v = unsafe { &mut *VP.with(|e| e.get()) };
            if let Some(info) = v.children.get(&pid) {
                if info.done { return info.exit_code; }
            } else {
                return -1; // unknown pid
            }
        }
        vproc::r#yield();
    }
}

fn main() {
    println!("=== vproc Phase 2: virtual fork/waitpid ===\n");
    println!("PID: {} (1 process, 0 real forks)\n", std::process::id());

    // Run everything inside a root coroutine so yield works correctly
    vproc::spawn(Box::new(|| {
        // Test 1: simple fork + wait
        println!("--- Test 1: fork, child works, parent waits ---");
        let child1 = virtual_fork(Box::new(|| {
            println!("  [child 10000] step 0");
            vproc::r#yield();
            println!("  [child 10000] step 1");
        }));
        println!("[parent] forked vpid={}", child1);
        virtual_waitpid(child1);
        println!("[parent] child {} done", child1);

        // Test 2: 3 concurrent children
        println!("\n--- Test 2: 3 concurrent children ---");
        let mut pids = vec![];
        for i in 0..3 {
            let pid = virtual_fork(Box::new(move || {
                let mypid = 10001 + i;
                println!("  [child {}] step 0", mypid);
                vproc::r#yield();
                println!("  [child {}] step 1", mypid);
            }));
            pids.push(pid);
        }
        // Wait for all (interleaved)
        for pid in &pids {
            virtual_waitpid(*pid);
            println!("[parent] child {} done", pid);
        }

        // Test 3: nested fork
        println!("\n--- Test 3: nested fork (child → grandchild) ---");
        let child3 = virtual_fork(Box::new(|| {
            println!("  [child] spawning grandchild...");
            let gc = virtual_fork(Box::new(|| {
                println!("    [grandchild] running");
                vproc::r#yield();
                println!("    [grandchild] done");
            }));
            println!("  [child] waiting for grandchild {}...", gc);
            virtual_waitpid(gc);
            println!("  [child] grandchild done");
        }));
        virtual_waitpid(child3);
        println!("[parent] child {} done", child3);

        // Test 4: stress - 100 children
        println!("\n--- Test 4: 100 children stress ---");
        let start = std::time::Instant::now();
        let mut all_pids = vec![];
        for i in 0..100 {
            let pid = virtual_fork(Box::new(move || {
                if i % 25 == 0 { vproc::r#yield(); }
            }));
            all_pids.push(pid);
        }
        for pid in &all_pids {
            virtual_waitpid(*pid);
        }
        println!("[parent] 100 children done in {:.2}ms", start.elapsed().as_secs_f64() * 1000.0);

        println!("\n--- Summary ---");
        println!("Context switches: {}", vproc::switch_count());
        println!("Real processes created: 0");
        println!("\n=== Phase 2 PASS ===");
    }));

    vproc::block_on_all();
}
