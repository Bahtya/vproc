//! Stress tests for hardening: rapid spawn/exit, large data, cache churn.

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
fn test_rapid_spawn_exit() {
    let mut parts = Vec::new();
    for i in 0..50 {
        parts.push(format!("echo line{}", i));
    }
    let cmd = parts.join("; ");
    let (ok, stdout, stderr) = run_vproc(&cmd);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    for i in 0..50 {
        assert!(stdout.contains(&format!("line{}", i)), "missing line{}: {}", i, stdout);
    }
}

#[test]
fn test_large_pipe_data() {
    let (ok, stdout, stderr) = run_vproc("yes | head -5000");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    let lines = stdout.lines().count();
    assert!(lines >= 4500, "expected ~5000 lines, got {}: {stderr}", lines);
}

#[test]
fn test_multi_stage_pipe_chain() {
    let (ok, stdout, stderr) = run_vproc("echo hello world | cat | cat | cat | cat | cat | wc -w");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.trim().ends_with('2'), "expected word count 2, got: {stdout}");
}

#[test]
fn test_sequential_different_commands() {
    // Run different binaries in sequence to exercise cache hit/miss paths
    let (ok, stdout, stderr) = run_vproc("echo a; echo b | cat; echo c; echo d | cat; echo e");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    for ch in &["a", "b", "c", "d", "e"] {
        assert!(stdout.contains(ch), "missing '{}': {}", ch, stdout);
    }
}

#[test]
fn test_nested_subshells() {
    let (ok, stdout, stderr) = run_vproc("echo $(( (echo deep) | cat ) | cat)");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
}

#[test]
fn test_many_pipes_interleaved() {
    // Interleave pipe and non-pipe commands
    let cmd = "echo a; echo b | cat; echo c; echo d | cat; echo e; echo f | cat";
    let (ok, stdout, stderr) = run_vproc(cmd);
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    for ch in &["a", "b", "c", "d", "e", "f"] {
        assert!(stdout.contains(ch), "missing '{}': {}", ch, stdout);
    }
}
