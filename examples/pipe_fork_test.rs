//! Test: pipe refcounting across fork'd fd tables.
//!
//! Verifies that closing pipe fds in one fd table doesn't break
//! the pipe for other fd tables sharing the same PipeBuffer.

fn main() {
    std::env::set_var("VPROC", "1");

    println!("=== pipe cross-table refcount test ===\n");

    // Simulate vpid 10 (parent) and vpid 20 (child)
    let parent = 10u32;
    let child = 20u32;

    // 1. Create pipe in parent's fd table
    let table = vproc::vfd::get_or_create_table(parent);
    let (read_fd, write_fd) = table.create_pipe();
    println!("1. parent pipe: read_fd={}, write_fd={}", read_fd, write_fd);

    // 2. Fork: copy fd table to child
    vproc::vfd::fork_fd_table(parent, child);
    println!("2. fork_fd_table({},{})", parent, child);

    // 3. Parent writes to pipe
    let table = vproc::vfd::get_table(parent).unwrap();
    if let Some(vproc::vfd::Vfd::PipeWrite(buf)) = table.get(write_fd) {
        let data = b"hello from parent!";
        let n = (**buf).write_to(data);
        println!("3. parent wrote {} bytes to pipe", n);
        assert_eq!(n, 18, "write should succeed");
    }

    // 4. Parent closes BOTH pipe fds (the bug trigger)
    let table = vproc::vfd::get_table(parent).unwrap();
    table.close(read_fd).unwrap();
    table.close(write_fd).unwrap();
    println!("4. parent closed both pipe fds (the bug trigger)");

    // 5. Child reads from pipe — should still work!
    let table = vproc::vfd::get_table(child).unwrap();
    if let Some(vproc::vfd::Vfd::PipeRead(buf)) = table.get(read_fd) {
        let mut dst = [0u8; 64];
        let n = (**buf).read_from(&mut dst);
        let s = std::str::from_utf8(&dst[..n as usize]).unwrap();
        println!("5. child read {} bytes: '{}'", n, s);
        assert_eq!(n, 18, "child should read 18 bytes");
        assert_eq!(s, "hello from parent!", "data should match");
    } else {
        panic!("child's read_fd not found in fd table!");
    }

    // 6. Child closes its write end (no more writers)
    let table = vproc::vfd::get_table(child).unwrap();
    table.close(write_fd).unwrap();
    println!("6. child closed its write end");

    // 7. Child reads again — should get EOF (no writers left)
    let table = vproc::vfd::get_table(child).unwrap();
    if let Some(vproc::vfd::Vfd::PipeRead(buf)) = table.get(read_fd) {
        let mut dst = [0u8; 64];
        let n = (**buf).read_from(&mut dst);
        println!("7. child read again: {} (EOF)", n);
        assert_eq!(n, 0, "should get EOF since all writers are gone");
    }

    println!("\n=== ALL PASS ===");
}
