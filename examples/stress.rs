use vproc;
use std::sync::atomic::{AtomicUsize, Ordering};

static COMPLETED: AtomicUsize = AtomicUsize::new(0);
static TOTAL_YIELDS: AtomicUsize = AtomicUsize::new(0);

fn main() {
    let n = 1000;
    let yields_per = 3;
    println!("=== vproc stress test: {} coroutines, {} yields each ===\n", n, yields_per);

    let start = std::time::Instant::now();

    for _i in 0..n {
        vproc::spawn(Box::new(move || {
            for _ in 0..yields_per {
                TOTAL_YIELDS.fetch_add(1, Ordering::Relaxed);
                vproc::r#yield();
            }
            COMPLETED.fetch_add(1, Ordering::Relaxed);
        }));
    }

    vproc::block_on_all();

    let elapsed = start.elapsed();
    let completed = COMPLETED.load(Ordering::Relaxed);
    let total_yields = TOTAL_YIELDS.load(Ordering::Relaxed);
    let switches = vproc::switch_count();

    println!("\n--- results ---");
    println!("coroutines:     {}", n);
    println!("completed:      {}/{}", completed, n);
    println!("total yields:   {}", total_yields);
    println!("context switches: {}", switches);
    println!("time:           {:.2}ms", elapsed.as_secs_f64() * 1000.0);
    println!("switch speed:   {:.0}ns/switch", elapsed.as_nanos() as f64 / switches as f64);
    println!("process count:  1");

    if completed == n {
        println!("\n=== PASS ===");
    } else {
        println!("\n=== FAIL: only {}/{} completed ===", completed, n);
    }
}
