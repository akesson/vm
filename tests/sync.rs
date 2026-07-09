//! End-to-end sync engine tests against local temp repos (no VMs needed —
//! these run in CI on Linux, Windows, and macOS, so the git plumbing is
//! exercised on every filesystem it will meet in real guests).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use vm::sync::{Snapshot, guest, host};

struct Repos {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    guest: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A host repo with a commit, plus an uninitialized guest checkout dir.
fn setup() -> Repos {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("host-repo");
    let guest_dir = tmp.path().join("guest-checkout");
    fs::create_dir_all(&host_dir).unwrap();

    git(&host_dir, &["init", "--quiet"]);
    git(&host_dir, &["config", "user.name", "test"]);
    git(&host_dir, &["config", "user.email", "test@local"]);
    git(&host_dir, &["config", "core.autocrlf", "false"]);

    write(&host_dir, "src/main.rs", "fn main() {}\n");
    write(&host_dir, ".gitignore", "target/\n*.log\n");
    git(&host_dir, &["add", "-A"]);
    git(&host_dir, &["commit", "--quiet", "-m", "initial"]);

    Repos {
        _tmp: tmp,
        host: host_dir,
        guest: guest_dir,
    }
}

/// One full sync: init guest, snapshot host, push, apply, verify tree hash.
fn sync(repos: &Repos) -> Snapshot {
    let guest_path = repos.guest.to_str().unwrap();
    guest::ensure_init(guest_path).unwrap();
    let snap = host::sync_to(&repos.host, "test", guest_path, None).unwrap();
    let guest_tree = guest::apply(guest_path, &snap.commit).unwrap();
    assert_eq!(guest_tree, snap.tree, "tree hash must round-trip");
    snap
}

#[test]
fn dirty_working_tree_arrives_byte_identical() {
    let repos = setup();
    // Uncommitted edit + brand-new untracked file: the whole point of the tool.
    write(&repos.host, "src/main.rs", "fn main() { /* dirty */ }\n");
    write(&repos.host, "src/new_module.rs", "pub fn nu() {}\n");

    sync(&repos);

    assert_eq!(
        fs::read_to_string(repos.guest.join("src/main.rs")).unwrap(),
        "fn main() { /* dirty */ }\n"
    );
    assert_eq!(
        fs::read_to_string(repos.guest.join("src/new_module.rs")).unwrap(),
        "pub fn nu() {}\n"
    );
}

#[test]
fn staging_area_is_untouched() {
    let repos = setup();
    // Carefully staged state, as in a `git add -p` session…
    write(&repos.host, "staged.txt", "staged content\n");
    git(&repos.host, &["add", "staged.txt"]);
    // …plus unstaged noise.
    write(&repos.host, "src/main.rs", "fn main() { /* changed */ }\n");

    let staged_before = git(&repos.host, &["diff", "--cached", "--stat"]);
    sync(&repos);
    let staged_after = git(&repos.host, &["diff", "--cached", "--stat"]);

    assert_eq!(
        staged_before, staged_after,
        "sync must not touch the real index"
    );
    assert!(staged_after.contains("staged.txt"));
    // And the guest still received both files.
    assert!(repos.guest.join("staged.txt").exists());
}

#[test]
fn deletes_propagate_and_guest_strays_are_removed() {
    let repos = setup();
    sync(&repos);
    assert!(repos.guest.join("src/main.rs").exists());

    // Delete on host; also drop a stray non-ignored file into the guest.
    fs::remove_file(repos.host.join("src/main.rs")).unwrap();
    write(&repos.guest, "stray.txt", "left over from another run\n");

    sync(&repos);

    assert!(
        !repos.guest.join("src/main.rs").exists(),
        "host delete must propagate"
    );
    assert!(
        !repos.guest.join("stray.txt").exists(),
        "guest strays must be cleaned"
    );
}

#[test]
fn guest_build_dir_survives_syncs() {
    let repos = setup();
    sync(&repos);

    // Simulate guest-local build state (ignored by .gitignore).
    write(
        &repos.guest,
        "target/debug/artifact.o",
        "expensive build output",
    );

    write(&repos.host, "src/main.rs", "fn main() { /* v2 */ }\n");
    sync(&repos);

    assert!(
        repos.guest.join("target/debug/artifact.o").exists(),
        "ignored build state must survive (incremental compilation)"
    );
}

#[test]
fn tracked_but_ignored_files_survive_snapshot() {
    let repos = setup();
    // A file that is tracked despite matching .gitignore (*.log).
    write(&repos.host, "important.log", "tracked despite ignore\n");
    git(&repos.host, &["add", "-f", "important.log"]);
    git(
        &repos.host,
        &["commit", "--quiet", "-m", "tracked ignored file"],
    );

    sync(&repos);

    assert!(
        repos.guest.join("important.log").exists(),
        "HEAD-seeded snapshot index must keep tracked-but-ignored files"
    );
}

#[test]
fn resync_of_unchanged_tree_is_identical_and_cheap() {
    let repos = setup();
    let first = sync(&repos);
    let second = sync(&repos);
    // Deterministic snapshot commit: same tree + parent → same sha, so the
    // push finds every object already present.
    assert_eq!(first, second);
}

#[test]
fn snapshot_reflects_edits_between_syncs() {
    let repos = setup();
    let first = sync(&repos);
    write(&repos.host, "src/main.rs", "fn main() { /* edited */ }\n");
    let second = sync(&repos);
    assert_ne!(first.tree, second.tree);
    assert_eq!(
        fs::read_to_string(repos.guest.join("src/main.rs")).unwrap(),
        "fn main() { /* edited */ }\n"
    );
}

#[test]
fn crlf_bytes_arrive_unconverted() {
    let repos = setup();
    write(&repos.host, "script.bat", "@echo off\r\nver\r\n");

    sync(&repos);

    let bytes = fs::read(repos.guest.join("script.bat")).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "@echo off\r\nver\r\n",
        "guest checkout must not translate line endings"
    );
}

#[test]
fn writeback_returns_guest_fixes_to_host() {
    let repos = setup();
    let base = sync(&repos);

    // Guest-side tool (think `clippy --fix`) rewrites a source file.
    write(
        &repos.guest,
        "src/main.rs",
        "fn main() { /* fixed by guest */ }\n",
    );
    let wb = guest::tree(repos.guest.to_str().unwrap()).unwrap();
    assert_ne!(wb.tree, base.tree);

    let applied =
        host::apply_writeback(&repos.host, repos.guest.to_str().unwrap(), &base, &wb, None)
            .unwrap();
    assert!(applied);
    assert_eq!(
        fs::read_to_string(repos.host.join("src/main.rs")).unwrap(),
        "fn main() { /* fixed by guest */ }\n"
    );
}

#[test]
fn writeback_of_unchanged_guest_is_a_noop() {
    let repos = setup();
    let base = sync(&repos);
    let wb = guest::tree(repos.guest.to_str().unwrap()).unwrap();
    assert_eq!(wb.tree, base.tree);
    let applied =
        host::apply_writeback(&repos.host, repos.guest.to_str().unwrap(), &base, &wb, None)
            .unwrap();
    assert!(!applied);
}

#[test]
#[cfg(unix)] // hook script; the bypass logic itself is platform-independent
fn sync_push_skips_pre_push_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let repos = setup();
    // A pre-push hook that always fails — like a repo whose hook runs the
    // full test suite (and might even recurse into `vm` itself). Sync pushes
    // are replication plumbing and must bypass it.
    let hook_dir = repos.host.join(".githooks");
    fs::create_dir_all(&hook_dir).unwrap();
    let hook = hook_dir.join("pre-push");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'pre-push hook must not run' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(&repos.host, &["config", "core.hooksPath", ".githooks"]);

    sync(&repos); // fails if the hook fires
}

#[test]
fn sync_works_in_repo_with_no_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("fresh");
    let guest_dir = tmp.path().join("fresh-guest");
    fs::create_dir_all(&host_dir).unwrap();
    git(&host_dir, &["init", "--quiet"]);
    git(&host_dir, &["config", "user.name", "test"]);
    git(&host_dir, &["config", "user.email", "test@local"]);
    write(&host_dir, "hello.txt", "no commits yet\n");

    let guest_path = guest_dir.to_str().unwrap();
    guest::ensure_init(guest_path).unwrap();
    let snap = host::sync_to(&host_dir, "test", guest_path, None).unwrap();
    let tree = guest::apply(guest_path, &snap.commit).unwrap();
    assert_eq!(tree, snap.tree);
    assert!(guest_dir.join("hello.txt").exists());
}
