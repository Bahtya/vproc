//! Integration tests for Phase 2 filesystem basics (issue #5).
//!
//! Tests for: open/openat/creat interception, Vfd::File tracking,
//! fstat/lseek passthrough, real fd cleanup.
//!
//! Note: sh optimizes the last command by exec'ing directly (no fork),
//! so glibc binaries fail. We avoid this by piping to a second command.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const TIMEOUT_SECS: u64 = 10;

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Temp file path in the writable target/debug directory.
/// Uses a per-test counter to avoid collisions when tests run in parallel.
fn tmp_path(name: &str) -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}/target/debug/vproc_ftest_{}_{}_{}",
        std::env::current_dir().unwrap().display(),
        std::process::id(),
        id,
        name
    )
}

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

// --- File open/read ---

#[test]
fn test_file_read_pipe() {
    // cat reads a real file, pipes through to another cat (forces fork)
    let (ok, stdout, stderr) = run_vproc("cat /proc/self/status | head -1");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Name:"), "missing 'Name:' in stdout: {stdout}");
}

#[test]
fn test_file_write_then_pipe() {
    let tmp = tmp_path("wr");
    // Write then read via pipe (cat | cat forces fork for both cats)
    let (ok, stdout, stderr) = run_vproc(&format!(
        "echo hello > {}; cat {} | cat",
        tmp, tmp
    ));
    let _ = std::fs::remove_file(&tmp);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello' in stdout: {stdout}");
}

#[test]
fn test_file_redirect_input() {
    let tmp = tmp_path("in");
    std::fs::write(&tmp, "test_data\n").unwrap();
    // Use pipe to force fork for cat
    let (ok, stdout, stderr) = run_vproc(&format!("cat < {} | cat", tmp));
    let _ = std::fs::remove_file(&tmp);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("test_data"), "missing 'test_data' in stdout: {stdout}");
}

// --- lseek ---

#[test]
fn test_file_seek_read() {
    // dd uses lseek internally
    let (ok, stdout, stderr) = run_vproc("echo abcdef | dd bs=1 skip=3 2>/dev/null");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("def"), "missing 'def' in stdout: {stdout}");
}

// --- Multiple file operations ---

#[test]
fn test_two_files_pipe() {
    let tmp1 = tmp_path("m1");
    let tmp2 = tmp_path("m2");
    let (ok, stdout, stderr) = run_vproc(&format!(
        "echo first > {}; echo second > {}; cat {} | cat; cat {} | cat",
        tmp1, tmp2, tmp1, tmp2
    ));
    let _ = std::fs::remove_file(&tmp1);
    let _ = std::fs::remove_file(&tmp2);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("first"), "missing 'first' in stdout: {stdout}");
    assert!(stdout.contains("second"), "missing 'second' in stdout: {stdout}");
}

#[test]
fn test_file_append() {
    let tmp = tmp_path("ap");
    let (ok, stdout, stderr) = run_vproc(&format!(
        "echo line1 > {}; echo line2 >> {}; cat {} | cat",
        tmp, tmp, tmp
    ));
    let _ = std::fs::remove_file(&tmp);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("line1"), "missing 'line1' in stdout: {stdout}");
    assert!(stdout.contains("line2"), "missing 'line2' in stdout: {stdout}");
}

// --- fstat (via wc) ---

#[test]
fn test_fstat_via_wc() {
    let (ok, stdout, stderr) = run_vproc("echo hello world | wc -w");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.trim().ends_with('2'), "expected word count 2, got: {stdout}");
}

// --- Stress: many file operations ---

#[test]
fn test_many_file_reads() {
    // Read /proc/self/status multiple times to stress file open/close
    let (ok, stdout, stderr) = run_vproc(
        "cat /proc/self/status | head -1; cat /proc/self/status | head -1; cat /proc/self/status | head -1"
    );
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    let count = stdout.matches("Name:").count();
    assert_eq!(count, 3, "expected 3 'Name:' lines, got {}: {stdout}", count);
}
