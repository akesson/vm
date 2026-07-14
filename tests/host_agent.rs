//! `drive_agent` — the host half of the liveness contract.
//!
//! One function carries every command vm has ever run in a guest: `vm exec` and
//! `vm run` differ in everything *around* it and in nothing about it. It writes
//! the request, holds the pipe open, beats on it for as long as the command
//! runs, and turns what comes back into an exit code. It is also where the
//! longest run of this project's bugs lived — the orphaned guest command (#21),
//! the transport death that became a silent `Ok(255)`, the `Child::wait()` that
//! closed the pipe it was supposed to hold open — and the half of the protocol
//! the agent's own tests cannot see, because they *are* the agent.
//!
//! It takes its transport as a `Command`, so it can be driven with one that runs
//! `vm _exec` right here. No VM, no Parallels, no ssh: a real host, a real agent,
//! and a real pipe between them.
//!
//! The requests below shorten the agent's silence budget, which shortens the
//! host's pulse with it (four beats to a budget, whatever the budget) — so a
//! minute of contract plays out in a second.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vm::exec::host::drive_agent;
use vm::proto::{ExecRequest, PROTO_VERSION};

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// The silence budget these tests give the agent, in place of the real minute.
/// Generous enough that a loaded CI box stalling a beat by most of a second
/// still does not read as a dead host.
const BUDGET_MS: u64 = 1000;

/// A transport that runs the agent locally — `prlctl exec`/`ssh` with everything
/// between the two processes taken out.
fn transport() -> Command {
    let mut cmd = Command::new(VM_BIN);
    cmd.arg("_exec");
    cmd
}

fn request(argv: &[&str], budget_ms: Option<u64>) -> ExecRequest {
    ExecRequest {
        version: PROTO_VERSION,
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: ".".to_string(),
        env: Default::default(),
        stdin: None,
        heartbeat_timeout_ms: budget_ms,
    }
}

/// A command that runs for about three seconds — three budgets' worth — and then
/// exits 0 of its own accord, having never read its stdin.
fn slow_command() -> Vec<String> {
    let argv: &[&str] = if cfg!(windows) {
        // Windows has no `sleep`, and `timeout` refuses a redirected stdin;
        // pinging loopback is the portable wait.
        &["cmd", "/C", "ping -n 4 127.0.0.1 >NUL"]
    } else {
        &["sh", "-c", "sleep 3"]
    };
    argv.iter().map(|s| s.to_string()).collect()
}

/// The whole point of the pulse: a command that outlives the agent's silence
/// budget several times over survives, because the host keeps saying it is here.
///
/// Without the beats the agent would tear the process tree down a budget in —
/// and the budget is a minute, so the failure this pins would look like "long
/// builds die at 60 seconds, sometimes".
#[test]
fn a_beating_host_holds_a_command_open_far_past_the_silence_budget() {
    let argv = slow_command();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let req = request(&argv, Some(BUDGET_MS));

    let started = Instant::now();
    let code = drive_agent("test", transport(), &req).expect("the agent answers");

    assert_eq!(code, 0, "the command was killed while the host was beating");
    assert!(
        started.elapsed() >= Duration::from_millis(BUDGET_MS),
        "the command has to have outlived the budget for this to prove anything"
    );
}

/// The guest command's exit code is the run's exit code — the contract every
/// task runner in front of vm depends on.
#[test]
fn the_guest_commands_exit_code_comes_back_as_it_is() {
    for expected in [0, 1, 7, 42] {
        let script = format!("exit {expected}");
        let argv: Vec<&str> = if cfg!(windows) {
            vec!["cmd", "/C", &script]
        } else {
            vec!["sh", "-c", &script]
        };
        let code = drive_agent("test", transport(), &request(&argv, Some(BUDGET_MS)))
            .expect("the agent answers");
        assert_eq!(code, expected, "exit {expected} came back as {code}");
    }
}

/// A command the guest cannot find is 127 — the guest's own answer, not vm's.
/// It must never be one of vm's reserved codes: a task runner reads 125 as "vm
/// hiccuped, retry" and would retry a typo forever.
#[test]
fn a_command_the_guest_cannot_find_is_127_and_not_an_infra_code() {
    let code = drive_agent(
        "test",
        transport(),
        &request(&["vm-test-definitely-not-an-executable"], Some(BUDGET_MS)),
    )
    .expect("the agent answers");

    assert_eq!(code, 127);
}

/// A transport that dies without a status of its own is vm's failure, not the
/// guest's. It used to come back as a silent `Ok(255)` — a number a caller could
/// not tell from a command that had genuinely exited 255, which is the whole
/// reason vm reserves its infra codes at all.
#[cfg(unix)]
#[test]
fn a_transport_killed_by_a_signal_is_an_infra_failure_not_an_exit_code() {
    // ssh cut down mid-command, or a prlctl killed with the terminal it ran in:
    // the transport is gone and the guest never reported anything.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("kill -9 $$")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let err = drive_agent("test", cmd, &request(&["true"], Some(BUDGET_MS)))
        .expect_err("a signalled transport must not pass for a guest exit code");

    let msg = err.to_string();
    assert!(
        msg.contains("killed before the guest reported an exit status"),
        "the error has to say what happened: {msg}"
    );
}

/// The pipe is the liveness channel, and `Child::wait()` closes the handle it is
/// given — which is why the handle is taken *out* of the child and held by the
/// heartbeat thread. Were it left in place, every command would read as a dead
/// host the moment vm sat down to wait for it. The command here is slower than
/// one beat, so a pipe closed at `wait()` would be a pipe closed while the agent
/// was still watching it.
#[test]
fn the_liveness_pipe_outlives_the_wait_that_would_have_closed_it() {
    let argv = slow_command();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();

    // No budget: the agent falls back to the real minute, so *only* an EOF can
    // kill this command — and an EOF is precisely what a closed pipe would send.
    let code = drive_agent("test", transport(), &request(&argv, None)).expect("the agent answers");

    assert_eq!(
        code, 0,
        "the command died without the host asking it to — the pipe closed under it"
    );
}
