//! The journal, end to end through the real binary.
//!
//! What is proved here is precisely what the unit tests in `vm::journal` cannot
//! reach: the process-global that `main` arms, and the rule deciding whether to
//! arm it at all. Every case drives the binary as a subprocess, so that global
//! lives in the child — which is the reason none of this is a unit test. A lib
//! test that armed the static would leak it into every other test sharing the
//! process under plain `cargo test`, and `cargo test` is a supported way to run
//! this suite (it is how it runs inside the Windows guest).
//!
//! No VM and no Parallels needed. Breadcrumbs come from `--or-native`, whose
//! fast path returns before the config is even loaded; errors and notes come
//! from a config that is not there.

use std::path::{Path, PathBuf};
use std::process::Command;

const VM_BIN: &str = env!("CARGO_BIN_EXE_vm");

/// The os literal that makes `--or-native` run the command here instead of
/// looking for a VM — so these tests work on all three CI runners.
fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// A temp dir standing in for `~/.config/vm`. The config file itself never has
/// to exist: `Config::path()` only has to *name* it, and the journal hangs off
/// its parent — the same derivation the lock dir already uses, which is what
/// makes `$VM_CONFIG` redirect the journal for free.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    (dir, config)
}

fn journal_of(dir: &Path) -> PathBuf {
    dir.join("log").join("vm.log")
}

/// Run `vm …` with `$VM_CONFIG` pointed into a temp dir. Returns exit code,
/// stdout and stderr — stdout because the journal must not cost vm the one
/// contract the README makes about it.
fn run(config: &Path, env: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(VM_BIN);
    cmd.args(args).env("VM_CONFIG", config);
    for (name, value) in env {
        cmd.env(name, value);
    }
    let out = cmd.output().expect("vm runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A command that runs natively on every host, as one argument — so vm treats
/// it as a script and hands it to the guest OS's shell.
const NATIVE_CMD: &str = "echo hi";

fn native_run(config: &Path, env: &[(&str, &str)], quiet: bool) -> (i32, String, String) {
    let mut args = vec![];
    if quiet {
        args.push("-q");
    }
    args.extend(["exec", host_os(), "--or-native", "--", NATIVE_CMD]);
    run(config, env, &args)
}

/// `2026-07-14T16:19:48.412+02:00 [8831] …` — the shape the whole exercise is
/// about, since the log this replaces carried no time at all. Checked by hand
/// rather than by regex: vm has no regex dependency and is not getting one for
/// a test.
fn stamp_and_pid(line: &str) -> Option<&str> {
    let (stamp, rest) = line.split_once(' ')?;
    let b = stamp.as_bytes();
    let shaped = stamp.len() == 29
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'.'
        && (b[23] == b'+' || b[23] == b'-')
        && b[26] == b':'
        && stamp[..4].bytes().all(|c| c.is_ascii_digit());
    if !shaped {
        return None;
    }
    let pid = rest.strip_prefix('[')?.split_once(']')?.0;
    if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(pid)
}

#[test]
fn every_journal_line_carries_the_time_it_happened_and_the_pid_that_wrote_it() {
    let (dir, config) = workspace();
    let (code, stdout, stderr) = native_run(&config, &[], false);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("vm ▸ native"), "the breadcrumb: {stderr}");

    let body = std::fs::read_to_string(journal_of(dir.path())).expect("a journal was written");
    let lines: Vec<&str> = body.lines().collect();
    assert!(!lines.is_empty(), "the journal is empty");
    for line in &lines {
        assert!(
            stamp_and_pid(line).is_some(),
            "unstamped journal line: {line}"
        );
    }
    assert!(
        body.contains("vm ▸ native"),
        "the line vm printed is the line vm kept: {body}"
    );
    // The whole point of the exercise, stated once: stdout is still only ever
    // the command's own.
    assert_eq!(stdout.trim(), "hi", "stdout is the command's: {stdout:?}");
}

#[test]
fn quiet_takes_the_breadcrumb_off_stderr_but_leaves_it_in_the_journal() {
    let (dir, config) = workspace();
    let (code, stdout, stderr) = native_run(&config, &[], true);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("vm ▸ native"),
        "-q silences narration on the terminal: {stderr}"
    );
    assert_eq!(stdout.trim(), "hi", "-q is not about the command's output");

    let body = std::fs::read_to_string(journal_of(dir.path())).expect("a journal was written");
    assert!(
        body.contains("vm ▸ native"),
        "a quiet run is still a run you can read back: {body}"
    );
}

/// `-q` suppresses narration, never news. A quiet flag that swallowed the note
/// explaining a command's form, or the error explaining its death, would be a
/// trap — and the launchd reap job runs `-q`, so this is the rule that keeps its
/// warnings reaching anyone at all.
#[test]
fn quiet_never_hides_a_note_or_an_error() {
    let (dir, config) = workspace();
    // No config at that path, so the alias cannot resolve — and the lone `&&`
    // draws an advisory on the way past.
    let (code, _, stderr) = run(
        &config,
        &[],
        &["-q", "exec", "lin", "--", "echo", "a", "&&", "echo", "b"],
    );
    assert_eq!(code, 2, "a missing config is a usage error: {stderr}");
    assert!(
        stderr.contains("vm ▸ note:"),
        "-q must not hide a note: {stderr}"
    );
    assert!(
        stderr.contains("vm: config error:"),
        "-q must not hide the failure: {stderr}"
    );

    let body = std::fs::read_to_string(journal_of(dir.path())).expect("a journal was written");
    assert!(body.contains("vm ▸ note:"), "{body}");
    assert!(body.contains("vm: config error:"), "{body}");
}

/// The agent half of vm runs *inside* a guest, where its stdout is a wire
/// protocol the host parses back and a log file would be stranded in a VM
/// nobody shells into. `_version` is the cheapest of the guest verbs.
#[test]
fn a_guest_verb_keeps_no_journal() {
    let (dir, config) = workspace();
    let (code, stdout, _) = run(&config, &[], &["_version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"proto\""), "the wire protocol: {stdout}");
    assert!(
        !dir.path().join("log").exists(),
        "the agent must leave no journal behind in a guest"
    );
}

/// The escape hatch. The journal keeps the command lines you ran, so there has
/// to be a way to say no — and saying no must not also cost you the terminal.
#[test]
fn vm_journal_off_writes_no_file_but_still_narrates() {
    let (dir, config) = workspace();
    let (code, _, stderr) = native_run(&config, &[("VM_JOURNAL", "off")], false);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("vm ▸ native"),
        "off silences the file, not the terminal: {stderr}"
    );
    assert!(
        !dir.path().join("log").exists(),
        "VM_JOURNAL=off must write nothing at all"
    );
}

/// One file, many runs — concurrent ones included, since `lock::shared` means
/// two `vm` processes on one VM is the normal case. The pid is what tells their
/// lines apart, and the stamp is what orders them.
#[test]
fn successive_runs_append_to_one_journal_and_are_told_apart_by_pid() {
    let (dir, config) = workspace();
    for _ in 0..2 {
        let (code, _, stderr) = native_run(&config, &[], false);
        assert_eq!(code, 0, "{stderr}");
    }
    let body = std::fs::read_to_string(journal_of(dir.path())).unwrap();
    let pids: std::collections::BTreeSet<&str> = body.lines().filter_map(stamp_and_pid).collect();
    assert_eq!(pids.len(), 2, "two runs, two pids, one file: {body}");
}
