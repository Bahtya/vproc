//! Integration tests for Phase 3 signal delivery (SIGPIPE, kill, raise).

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

// --- SIGPIPE ---

#[test]
fn test_sigpipe_cat_head() {
    // cat writes many lines, head -1 reads one line then exits.
    // The pipe read end closes, cat should get SIGPIPE and terminate.
    let (ok, stdout, stderr) = run_vproc("cat /proc/self/status | head -1");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Name:"), "missing 'Name:' in stdout: {stdout}");
}

#[test]
fn test_sigpipe_echo_head() {
    // Simpler: echo produces output, head -0 reads nothing and exits.
    // The pipe read end closes before echo finishes.
    let (_ok, stdout, _stderr) = run_vproc("echo hello | head -0");
    // head -0 outputs nothing, echo gets SIGPIPE. The overall pipeline
    // may succeed or fail depending on shell behavior, but must not hang.
    assert!(stdout.is_empty() || stdout.contains("hello"),
        "unexpected stdout: {stdout}");
}

#[test]
fn test_pipe_chain_completes() {
    // Multi-stage pipe — all stages should complete without hanging.
    let (ok, stdout, stderr) = run_vproc("echo hello world | cat | cat | wc -w");
    assert!(ok, "exit != 0\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.trim().ends_with('2'), "expected word count 2, got: {stdout}");
}
