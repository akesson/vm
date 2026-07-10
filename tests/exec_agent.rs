//! The `vm _exec` agent driven exactly as the host drives it: one JSON line
//! on stdin, stdin held open for the lifetime of the command (the liveness
//! channel), exit code propagated. Runs in CI on all three OSes.

use std::io::Write;
use std::process::{Command, Stdio};

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// Drive `vm _exec` like the host does; returns (exit_code, stdout).
fn agent_exec(argv: &[&str], cwd: &str) -> (i32, String) {
    let req = serde_json::json!({
        "version": 1,
        "argv": argv,
        "cwd": cwd,
        "shell": false,
    });
    let mut child = Command::new(VM_BIN)
        .arg("_exec")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agent spawns");
    // Hold stdin open across wait, exactly like the host (Child::wait would
    // otherwise close it and trip the liveness watcher).
    let mut stdin = child.stdin.take().expect("piped stdin");
    writeln!(stdin, "{req}").unwrap();
    let out = child.wait_with_output().expect("agent runs");
    drop(stdin);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

#[test]
fn runs_argv_in_cwd_and_captures_output() {
    let tmp = tempfile::tempdir().unwrap();
    // Self-spawn: the vm binary itself is the only executable guaranteed to
    // exist on every CI OS. `_version` prints a JSON line.
    let (code, stdout) = agent_exec(&[VM_BIN, "_version"], tmp.path().to_str().unwrap());
    assert_eq!(code, 0);
    assert!(stdout.contains("\"proto\":1"), "stdout: {stdout}");
}

#[test]
fn propagates_nonzero_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    // `vm definitely-not-a-verb` exits 2 (clap usage error).
    let (code, _) = agent_exec(
        &[VM_BIN, "definitely-not-a-verb"],
        tmp.path().to_str().unwrap(),
    );
    assert_eq!(code, 2);
}

#[test]
fn missing_cwd_is_an_infra_error() {
    // A working directory that doesn't exist means the sync never landed — a vm
    // infra failure, so the agent exits with the reserved 125, not a guest code.
    let (code, _) = agent_exec(&[VM_BIN, "_version"], "/definitely/not/a/dir");
    assert_eq!(code, 125);
}

#[test]
fn command_not_found_is_127_not_infra() {
    let tmp = tempfile::tempdir().unwrap();
    // A command that doesn't exist is the *command's* own result (shell code
    // 127), never vm's reserved infra code — the guest must not let 125 leak
    // back to the host as if it were a real exit status.
    let (code, _) = agent_exec(
        &["vm-test-definitely-not-an-executable"],
        tmp.path().to_str().unwrap(),
    );
    assert_eq!(code, 127);
}

#[test]
fn liveness_stdin_eof_kills_the_child_tree() {
    let req = serde_json::json!({
        "version": 1,
        "argv": [VM_BIN, "_exec"], // grandchild that blocks forever reading stdin… (never gets a request)
        "cwd": ".",
        "shell": false,
    });
    let mut child = Command::new(VM_BIN)
        .arg("_exec")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("agent spawns");
    let mut stdin = child.stdin.take().expect("piped stdin");
    writeln!(stdin, "{req}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Simulate host death / connection drop: close the liveness channel.
    drop(stdin);
    // The watcher must tear the agent (and its child) down promptly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert_ne!(status.code(), Some(0), "should exit via emergency stop");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent did not stop within 10s of stdin EOF"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
