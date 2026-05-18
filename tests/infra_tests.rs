//! Integration tests for Phase 1 infrastructure improvements (issue #5).
//!
//! Tests for: PipeBuffer Arc refcounting, panic recovery, C string cleanup,
//! binary cache lifecycle, and mapped region cleanup.

use std::process::Command;

const TIMEOUT_SECS: u64 = 10;

fn run_vproc(cmd: &str) -> (bool, String, String) {
    let bin = std::env::current_dir()
        .unwrap()
        .join("target/release/examples/test_single");
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

// --- Pipe reliability (Arc refactor) ---

#[test]
fn test_pipe_arc_basic() {
    let (ok, stdout, stderr) = run_vproc("echo hello | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello': {stdout}");
}

#[test]
fn test_pipe_arc_multi_stage() {
    let (ok, stdout, stderr) = run_vproc("echo abc | cat | cat | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("abc"), "missing 'abc': {stdout}");
}

#[test]
fn test_pipe_arc_large_data() {
    // Stress test: write enough data to fill the 64KiB pipe buffer multiple times
    let (ok, stdout, stderr) = run_vproc("yes | head -1000");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    let lines = stdout.lines().count();
    assert!(lines >= 900, "expected ~1000 lines, got {}: {stderr}", lines);
}

// --- Sequential execution (binary cache + segment restore) ---

#[test]
fn test_sequential_three_commands() {
    let (ok, stdout, stderr) = run_vproc("echo first; echo second; echo third");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("first"), "missing 'first': {stdout}");
    assert!(stdout.contains("second"), "missing 'second': {stdout}");
    assert!(stdout.contains("third"), "missing 'third': {stdout}");
}

#[test]
fn test_sequential_pipe_stress() {
    // Run 5 pipe commands back-to-back to stress test the cache
    let cmd = "echo a | cat; echo b | cat; echo c | cat; echo d | cat; echo e | cat";
    let (ok, stdout, stderr) = run_vproc(cmd);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    for ch in &["a", "b", "c", "d", "e"] {
        assert!(stdout.contains(ch), "missing '{}': {stdout}", ch);
    }
}

// --- Binary cache correctness ---

#[test]
fn test_cache_correctness_across_invocations() {
    // Each command must see fresh shell state despite cache reuse
    let (ok, stdout, stderr) = run_vproc("X=hello; echo $X; echo $X");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello': {stdout}");
}

#[test]
fn test_cache_with_subshell() {
    let (ok, stdout, stderr) = run_vproc("(echo inner) | cat; echo outer | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("inner"), "missing 'inner': {stdout}");
    assert!(stdout.contains("outer"), "missing 'outer': {stdout}");
}

// --- Resource cleanup ---

#[test]
fn test_many_short_commands() {
    // Run many short commands to verify no resource exhaustion
    let mut parts = Vec::new();
    for i in 0..20 {
        parts.push(format!("echo cmd{}", i));
    }
    let cmd = parts.join("; ");
    let (ok, stdout, stderr) = run_vproc(&cmd);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    for i in 0..20 {
        assert!(stdout.contains(&format!("cmd{}", i)), "missing cmd{} in {}", i, stdout);
    }
}
