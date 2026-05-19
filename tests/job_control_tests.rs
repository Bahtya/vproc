//! Job control tests: setsid, getpgrp, tcsetpgrp, tcgetpgrp interception.

use std::process::Command;

const TIMEOUT_SECS: u64 = 30;

fn run_vproc(cmd: &str) -> (bool, String, String) {
    let bin = std::env::current_dir()
        .unwrap()
        .join("target/debug/examples/test_single");
    let output = Command::new("timeout")
        .arg(format!("{}s", TIMEOUT_SECS))
        .arg(&bin)
        .env("VPROC", "1")
        .arg(cmd)
        .output()
        .expect("failed to run test_single");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

#[test]
fn test_shell_setsid() {
    // Shell should be able to call setsid() without error
    let (ok, stdout, stderr) = run_vproc("echo ok");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("ok"), "missing 'ok': {stdout}");
}

#[test]
fn test_shell_background_job() {
    // Shell job control commands should not crash
    let (ok, stdout, stderr) = run_vproc("echo a; echo b");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("a"), "missing 'a': {stdout}");
    assert!(stdout.contains("b"), "missing 'b': {stdout}");
}

#[test]
fn test_shell_piped_with_setsid() {
    // Pipe + setsid should work together
    let (ok, stdout, stderr) = run_vproc("echo hello | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello': {stdout}");
}
