fn main() {
    eprintln!("spawning 1 coroutine (1 yield)");
    vproc::spawn(Box::new(|| {
        eprintln!("[0] before yield");
        vproc::r#yield();
        eprintln!("[0] after yield");
    }));
    vproc::block_on_all();
    eprintln!("done, switches={}", vproc::switch_count());
}
