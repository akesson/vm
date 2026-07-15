//! A reader that stops reading must not kill vm (#36).
//!
//! `vm doctor | grep -q prlctl` exited **101 and printed nothing** — grep closes
//! the pipe the moment it matches, vm was still writing, and Rust turns the
//! closed reader into an `EPIPE` write error that `println!`/`eprintln!` panic
//! on. The panic message went to the pipe that had just closed, so the failure
//! could not even report itself.
//!
//! The read end is closed *before* vm writes a byte, which is what makes these
//! deterministic: with no reader left, the first write fails, every time. A test
//! that spawned a reader and raced it would pass on a fast machine and rot.
//!
//! No VM and no Parallels: `--or-native` narrates on stderr without one, and
//! `_version` answers on stdout without one.

use std::io::pipe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// The exit code of a Rust process that panicked. The bug, in one number.
const PANIC: i32 = 101;

fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// A temp dir standing in for `~/.config/vm` — the journal hangs off it, and a
/// panic is exactly what the journal is there to have caught.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    (dir, config)
}

fn journal_body(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log").join("vm.log")).unwrap_or_default()
}

/// Which stream to hand vm a pipe nobody is reading.
enum Closed {
    Stdout,
    Stderr,
}

/// Run `vm …` with one stream wired to a pipe whose read end is already gone.
fn run_into_a_closed_pipe(config: &Path, closed: Closed, args: &[&str]) -> i32 {
    let (reader, writer) = pipe().expect("a pipe");
    let mut cmd = Command::new(VM_BIN);
    cmd.args(args).env("VM_CONFIG", config);
    match closed {
        Closed::Stdout => cmd.stdout(Stdio::from(writer)).stderr(Stdio::null()),
        Closed::Stderr => cmd.stderr(Stdio::from(writer)).stdout(Stdio::null()),
    };
    let mut child = cmd.spawn().expect("vm runs");

    // The write end now belongs to the child alone. Drop the only reader and
    // every write it makes is an EPIPE.
    drop(reader);

    child.wait().expect("vm exits").code().unwrap_or(-1)
}

/// The headline repro. vm narrates on stderr, so a `| grep -q` on a *combined*
/// stream is what closed it — and the run's own exit code is what the caller
/// deserves back, not a panic.
#[test]
fn a_closed_stderr_does_not_kill_a_run_that_succeeded() {
    let (dir, config) = workspace();
    let code = run_into_a_closed_pipe(
        &config,
        Closed::Stderr,
        &["exec", host_os(), "--or-native", "--", "echo hi"],
    );

    assert_ne!(code, PANIC, "a closed reader panicked vm");
    assert_eq!(code, 0, "the command succeeded, so vm must say so");

    // The journal is the one witness a closed stderr cannot silence — and the
    // only reason this bug was ever seen. It must hold no panic.
    let body = journal_body(dir.path());
    assert!(!body.contains("panic"), "vm panicked: {body}");
}

/// `vm doctor` is the command the issue was filed against: ~30 lines, so a
/// reader that stops early is still reading when vm is still writing. Its exit
/// code depends on the host it runs on (0 where Parallels is installed and
/// healthy, nonzero on a CI runner with no prlctl at all) — the contract under
/// test is only that vm does not *panic*, and reports whatever it found.
#[test]
fn a_closed_stderr_does_not_kill_doctor() {
    let (dir, config) = workspace();
    let code = run_into_a_closed_pipe(&config, Closed::Stderr, &["doctor"]);

    assert_ne!(code, PANIC, "vm doctor | grep -q … panicked vm");
    let body = journal_body(dir.path());
    assert!(!body.contains("panic"), "vm panicked: {body}");
}

/// The stdout half of the same bug: `vm ls | head -1` closes the table on its
/// second row. `_version` stands in for it here because it is the one verb that
/// prints to stdout without needing a VM — the table itself gets the same
/// treatment against a fake prlctl.
#[test]
fn a_closed_stdout_does_not_kill_a_verb_that_prints_to_it() {
    let (_dir, config) = workspace();
    let code = run_into_a_closed_pipe(&config, Closed::Stdout, &["_version"]);

    assert_ne!(code, PANIC, "a closed reader panicked vm");
    assert_eq!(code, 0, "reporting the version is not a failure");
}
