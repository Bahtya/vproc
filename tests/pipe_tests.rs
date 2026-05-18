//! Integration tests for pipe/fork fix (issue #1).
//!
//! Each test spawns `test_single` as a subprocess with a timeout,
//! because the tests involve real fork() which is incompatible
//! with cargo test's thread model.

use std::process::Command;

const TIMEOUT_SECS: u64 = 10;

fn run_vproc(cmd: &str) -> (bool, String, String) {
    let bin = std::env::current_dir()
        .unwrap()
        .join("target/release/examples/test_single");
    let output = Command::new("timeout")
        .arg(format!("{}s", TIMEOUT_SECS))
        .arg(bin)
        .arg(cmd)
        .env("VPROC", "1")
        .output()
        .expect("failed to run test_single");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();
    (success, stdout, stderr)
}

#[test]
fn test_simple_echo() {
    let (ok, stdout, stderr) = run_vproc("echo hello");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello' in stdout: {stdout}");
}

#[test]
fn test_sequential_builtins() {
    let (ok, stdout, stderr) = run_vproc("echo hello; echo world");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello': {stdout}");
    assert!(stdout.contains("world"), "missing 'world': {stdout}");
}

#[test]
fn test_pipe_builtin_builtin() {
    // Was SIGSEGV before fix: echo (builtin) | true (builtin)
    let (ok, stdout, stderr) = run_vproc("echo hello | true");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
}

#[test]
fn test_pipe_builtin_cat() {
    // Was SIGSEGV before fix: echo (builtin) | cat (external)
    let (ok, stdout, stderr) = run_vproc("echo hello | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello' in stdout: {stdout}");
}

#[test]
fn test_multi_stage_pipe() {
    let (ok, stdout, stderr) = run_vproc("echo hello world | cat | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello world"), "missing 'hello world': {stdout}");
}

#[test]
fn test_subshell_pipe() {
    let (ok, stdout, stderr) = run_vproc("(echo a; echo b) | cat");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("a"), "missing 'a': {stdout}");
    assert!(stdout.contains("b"), "missing 'b': {stdout}");
}

#[test]
fn test_pipe_with_grep() {
    let (ok, stdout, stderr) = run_vproc("printf 'foo\\nbar\\nbaz\\n' | grep ba");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("bar") && stdout.contains("baz"), "missing grep output: {stdout}");
}

#[test]
fn test_sequential_pipe_invocations() {
    // Run two pipe commands back to back in the same process.
    // This tests that the binary cache correctly restores writable segments
    // so the shell's global state is fresh for the second invocation.
    let bin = std::env::current_dir()
        .unwrap()
        .join("target/release/examples/test_single");
    let output = Command::new("timeout")
        .arg(format!("{}s", TIMEOUT_SECS))
        .arg(bin)
        .arg("echo hello | cat")
        .arg("echo world | cat")
        .env("VPROC", "1")
        .output()
        .expect("failed to run test_single");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("hello"), "missing 'hello': {stdout}");
    assert!(stdout.contains("world"), "missing 'world': {stdout}");
}
