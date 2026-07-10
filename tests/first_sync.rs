//! First-sync hook semantics, driving the guest verb (`vm::exec::guest::first_sync`)
//! directly against a temp checkout — no VM needed. Gated to unix because the
//! hook strings here are POSIX (`echo`, `exit 3`, `true`); the marker logic the
//! tests exercise is OS-agnostic, and the Windows `cmd /C` path is covered by
//! the end-to-end verification against a real guest.
#![cfg(unix)]

use std::path::Path;
use vm::exec::guest::first_sync;
use vm::sync::guest;

fn line_count(p: &Path) -> usize {
    std::fs::read_to_string(p)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// A checkout dir with an initialized `.git` (as the first real sync would leave it).
fn checkout() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    guest::ensure_init(dir.to_str().unwrap()).unwrap();
    (tmp, dir)
}

#[test]
fn runs_once_then_no_ops_and_reruns_when_marker_cleared() {
    let (_tmp, dir) = checkout();
    let path = dir.to_str().unwrap();
    let log = dir.join("ran.log");
    let marker = dir.join(".git").join("vm-first-sync-done");

    // First run: the hook executes and success is recorded.
    assert_eq!(first_sync(path, "echo x >> ran.log").unwrap(), 0);
    assert!(
        marker.exists(),
        "marker must be written after a successful hook"
    );
    assert_eq!(line_count(&log), 1);

    // Second run: marker present ⇒ no-op, the hook must NOT run again.
    assert_eq!(first_sync(path, "echo x >> ran.log").unwrap(), 0);
    assert_eq!(line_count(&log), 1, "hook re-ran while the marker existed");

    // Clearing the marker (what `vm clean` / any checkout recreation does) re-runs it.
    std::fs::remove_file(&marker).unwrap();
    assert_eq!(first_sync(path, "echo x >> ran.log").unwrap(), 0);
    assert_eq!(line_count(&log), 2);
    assert!(marker.exists());
}

#[test]
fn failing_hook_propagates_code_and_writes_no_marker() {
    let (_tmp, dir) = checkout();
    let path = dir.to_str().unwrap();
    let marker = dir.join(".git").join("vm-first-sync-done");

    // Nonzero exit is propagated and leaves no marker, so the hook retries.
    assert_eq!(first_sync(path, "exit 3").unwrap(), 3);
    assert!(!marker.exists(), "a failed hook must not be marked done");

    // A later succeeding hook then marks it.
    assert_eq!(first_sync(path, "true").unwrap(), 0);
    assert!(marker.exists());
}

#[test]
fn no_git_checkout_is_a_silent_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("never-synced");
    std::fs::create_dir_all(&dir).unwrap(); // dir exists, but no `.git`

    // Nothing to set up yet — skip without running the hook.
    assert_eq!(
        first_sync(dir.to_str().unwrap(), "echo boom > proof").unwrap(),
        0
    );
    assert!(
        !dir.join("proof").exists(),
        "hook ran without a .git checkout"
    );
}
