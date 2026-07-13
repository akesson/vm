//! The `vm _exec` agent driven exactly as the host drives it: one JSON line
//! on stdin, stdin held open for the lifetime of the command (the liveness
//! channel), exit code propagated. Runs in CI on all three OSes.
//!
//! The request is always a plain argv — the host composes any shell invocation
//! before it goes on the wire, so a *script* reaches the agent already wrapped
//! in `sh -c` / `cmd /C`, and these tests send it the same way.

use std::io::Write;
use std::process::{Command, Stdio};
use vm::proto::PROTO_VERSION;

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// Drive `vm _exec` like the host does; returns (exit_code, stdout).
fn agent_exec(argv: &[&str], cwd: &str) -> (i32, String) {
    agent_exec_env(argv, cwd, &[])
}

/// `agent_exec` with `env` forwarded — what a `-e NAME=value` run puts on the wire.
fn agent_exec_env(argv: &[&str], cwd: &str, env: &[(&str, &str)]) -> (i32, String) {
    agent_request(argv, cwd, env, None)
}

/// `agent_exec` with a stdin payload — what `vm run … < script` puts on the wire.
fn agent_exec_stdin(argv: &[&str], cwd: &str, stdin: &str) -> (i32, String) {
    agent_request(argv, cwd, &[], Some(stdin))
}

fn agent_request(
    argv: &[&str],
    cwd: &str,
    env: &[(&str, &str)],
    stdin: Option<&str>,
) -> (i32, String) {
    let env: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect();
    let mut req = serde_json::json!({
        "version": PROTO_VERSION,
        "argv": argv,
        "env": env,
        "cwd": cwd,
    });
    if let Some(stdin) = stdin {
        req["stdin"] = serde_json::Value::String(stdin.to_string());
    }
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

#[cfg(unix)]
#[test]
fn forwarded_env_reaches_the_guest_process() {
    let tmp = tempfile::tempdir().unwrap();
    // The shell arrives as argv (the host wraps a script itself), and `printf`
    // omits the trailing newline, so the assertion is exact.
    let (code, stdout) = agent_exec_env(
        &["sh", "-c", "printf %s $FOO"],
        tmp.path().to_str().unwrap(),
        &[("FOO", "bar")],
    );
    assert_eq!(code, 0);
    assert_eq!(stdout, "bar", "stdout: {stdout:?}");
}

#[cfg(windows)]
#[test]
fn forwarded_env_reaches_the_guest_process() {
    let tmp = tempfile::tempdir().unwrap();
    // `cmd /C echo %FOO%` expands the forwarded value (trailing CRLF trimmed).
    let (code, stdout) = agent_exec_env(
        &["cmd", "/C", "echo %FOO%"],
        tmp.path().to_str().unwrap(),
        &[("FOO", "bar")],
    );
    assert_eq!(code, 0);
    assert_eq!(stdout.trim_end(), "bar", "stdout: {stdout:?}");
}

#[test]
fn runs_argv_in_cwd_and_captures_output() {
    let tmp = tempfile::tempdir().unwrap();
    // Self-spawn: the vm binary itself is the only executable guaranteed to
    // exist on every CI OS. `_version` prints a JSON line.
    let (code, stdout) = agent_exec(&[VM_BIN, "_version"], tmp.path().to_str().unwrap());
    assert_eq!(code, 0);
    assert!(
        stdout.contains(&format!("\"proto\":{PROTO_VERSION}")),
        "stdout: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn argv_is_never_run_through_a_shell() {
    // The byte-fidelity promise the arity rule leans on: in exec form an
    // argument holding shell syntax is data, not code. Were the agent to grow a
    // shell back, this argument would be word-split at the `|`, `$(…)` would be
    // substituted, and `>` would redirect — instead printf prints it verbatim.
    let tmp = tempfile::tempdir().unwrap();
    let literal = "a|b && $(echo pwned) > /dev/null";
    let (code, stdout) = agent_exec(&["printf", "%s", literal], tmp.path().to_str().unwrap());
    assert_eq!(code, 0);
    assert_eq!(stdout, literal, "stdout: {stdout:?}");
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

// ── the stdin payload (`vm run … < script`) ──────────────────────────────────

#[cfg(unix)]
#[test]
fn a_stdin_payload_reaches_the_command_and_then_ends() {
    // The `vm run lin -- sh < step.sh` shape: `sh` reads the script from stdin,
    // runs it, and *terminates* — which it only does on EOF, so this pins the
    // close as much as the write.
    let tmp = tempfile::tempdir().unwrap();
    let (code, stdout) = agent_exec_stdin(
        &["sh"],
        tmp.path().to_str().unwrap(),
        "printf payload-ran\nexit 7\n",
    );
    assert_eq!(code, 7, "the script's own exit code comes back");
    assert_eq!(stdout, "payload-ran", "stdout: {stdout:?}");
}

#[cfg(windows)]
#[test]
fn a_stdin_payload_reaches_the_command_and_then_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, stdout) = agent_exec_stdin(
        &["findstr", "."],
        tmp.path().to_str().unwrap(),
        "payload-ran\r\n",
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("payload-ran"), "stdout: {stdout:?}");
}

#[cfg(unix)]
#[test]
fn a_payload_survives_the_bytes_that_would_end_a_json_line() {
    // The payload rides *inside* the request's single JSON line, so newlines —
    // which every script is made of — must survive the trip escaped and come
    // back out intact.
    let tmp = tempfile::tempdir().unwrap();
    let script = "printf 'a\\n'\nprintf \"b'c\\n\"\nprintf 'uni-→\\n'\n";
    let (code, stdout) = agent_exec_stdin(&["sh"], tmp.path().to_str().unwrap(), script);
    assert_eq!(code, 0);
    assert_eq!(stdout, "a\nb'c\nuni-→\n", "stdout: {stdout:?}");
}

#[cfg(unix)]
#[test]
fn a_big_payload_into_a_command_that_never_reads_it_does_not_deadlock() {
    // The reason the payload is written on a thread. 1 MiB against a ~64 KiB
    // pipe buffer blocks the writer until the child drains it — and this child
    // never reads a byte. Written inline, the agent would hang here forever;
    // written on a thread, the child exits, the pipe breaks, and the write is
    // abandoned. (The `vm run lin -- 'apt-get update' < script` slip, which must
    // fail with apt's own exit code and not by wedging.)
    let tmp = tempfile::tempdir().unwrap();
    let payload = "x".repeat(1024 * 1024);
    let (code, _) = agent_exec_stdin(
        &["sh", "-c", "exit 7"],
        tmp.path().to_str().unwrap(),
        &payload,
    );
    assert_eq!(code, 7, "the command's own exit code, not a hang");
}

#[cfg(unix)]
#[test]
fn a_command_with_no_payload_reads_the_null_device() {
    // Every `vm exec` sends no payload, and its command's stdin must be at EOF
    // immediately — not an open pipe nobody will ever write to, which would hang
    // anything that reads stdin.
    let tmp = tempfile::tempdir().unwrap();
    let (code, stdout) = agent_exec(
        &["sh", "-c", "printf got:[%s] \"$(cat)\""],
        tmp.path().to_str().unwrap(),
    );
    assert_eq!(code, 0);
    assert_eq!(stdout, "got:[]", "stdout: {stdout:?}");
}

#[test]
fn liveness_stdin_eof_kills_the_child_tree() {
    let req = serde_json::json!({
        "version": PROTO_VERSION,
        "argv": [VM_BIN, "_exec"], // grandchild that blocks forever reading stdin… (never gets a request)
        "cwd": ".",
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
