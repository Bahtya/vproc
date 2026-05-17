fn main() {
    println!("=== vproc Phase 3: dynamic binary via dlopen ===\n");

    let test_binary = "/data/data/com.termux/files/usr/tmp/test_dynamic_hello";

    let result = vproc::vexec::virtual_execve_dynamic(
        test_binary,
        vec![test_binary.to_string()],
        vec![],
    );
    match result {
        Ok(exec) => {
            println!("spawned vpid = {}", exec.vpid);
            vproc::block_on_all();
            println!("coroutine completed");
        }
        Err(e) => {
            println!("error: {}", e);
        }
    }
}
