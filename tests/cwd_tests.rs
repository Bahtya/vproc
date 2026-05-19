//! Integration tests for Phase 3 per-coroutine cwd (Issue #7).

use std::process::Command;

const TIMEOUT_SECS: u64 = 10;

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

// --- chdir + getcwd ---

#[test]
fn test_chdir_pwd() {
    let (ok, stdout, stderr) = run_vproc("cd /data && pwd");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("/data"), "expected /data in stdout: {stdout}");
}

#[test]
fn test_chdir_relative_open() {
    // cd to /proc then open a relative path "self/status"
    let (ok, stdout, stderr) = run_vproc("cd /proc && cat self/status | head -1");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Name:"), "missing 'Name:' in stdout: {stdout}");
}

#[test]
fn test_chdir_parent() {
    // cd to /proc/self then cd .. and verify we're in /proc
    let (ok, stdout, stderr) = run_vproc("cd /proc/self && cd .. && pwd");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.trim() == "/proc", "expected /proc, got: {stdout}");
}

#[test]
fn test_chdir_two_dirs() {
    // cd to two different directories and verify pwd changes each time
    let (ok, stdout, stderr) = run_vproc("cd /data && pwd; cd /proc && pwd");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(lines.len() >= 2, "expected 2 lines, got: {stdout}");
    assert!(lines[0].contains("/data"), "first line should be /data: {}", lines[0]);
    assert!(lines[1].contains("/proc"), "second line should be /proc: {}", lines[1]);
}
