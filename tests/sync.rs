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
    sync_with(repos, &[])
}

/// A sync that also force-includes `extra` (the `--with-file` paths).
fn sync_with(repos: &Repos, extra: &[&str]) -> Snapshot {
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let guest_path = repos.guest.to_str().unwrap();
    guest::ensure_init(guest_path).unwrap();
    let snap = host::sync_to(&repos.host, "test", guest_path, None, &extra).unwrap();
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

/// Writeback is a patch applied to a working tree the host has kept editing for
/// the whole run — a long `vm claude` is minutes of it. Where the two touched
/// the same lines, the honest outcome is to refuse and say so: half-applying a
/// patch over the user's own edit would be the one failure they could not undo.
#[test]
fn writeback_over_a_conflicting_host_edit_is_refused_and_names_the_cause() {
    let repos = setup();
    let base = sync(&repos);

    // The guest fixes the file…
    write(
        &repos.guest,
        "src/main.rs",
        "fn main() { /* fixed by guest */ }\n",
    );
    let wb = guest::tree(repos.guest.to_str().unwrap()).unwrap();

    // …while the host edits the very same line, after the snapshot was taken.
    write(
        &repos.host,
        "src/main.rs",
        "fn main() { /* edited on the host meanwhile */ }\n",
    );

    let err = host::apply_writeback(&repos.host, repos.guest.to_str().unwrap(), &base, &wb, None)
        .expect_err("a conflicting writeback must not be forced onto the host tree");
    assert!(
        err.to_string().contains("host tree changed during the run"),
        "the error has to name the cause: {err}"
    );
    // And the host's own edit is exactly as they left it.
    assert_eq!(
        fs::read_to_string(repos.host.join("src/main.rs")).unwrap(),
        "fn main() { /* edited on the host meanwhile */ }\n",
        "a refused writeback must leave the working tree untouched"
    );
}

/// The other half of that rule: a host edit *elsewhere* in the tree is not a
/// conflict, and must not cost the user their writeback.
#[test]
fn writeback_lands_alongside_a_host_edit_to_another_file() {
    let repos = setup();
    let base = sync(&repos);

    write(
        &repos.guest,
        "src/main.rs",
        "fn main() { /* fixed by guest */ }\n",
    );
    let wb = guest::tree(repos.guest.to_str().unwrap()).unwrap();

    write(&repos.host, "notes.md", "written while the guest worked\n");

    let applied =
        host::apply_writeback(&repos.host, repos.guest.to_str().unwrap(), &base, &wb, None)
            .unwrap();
    assert!(applied);
    assert_eq!(
        fs::read_to_string(repos.host.join("src/main.rs")).unwrap(),
        "fn main() { /* fixed by guest */ }\n",
        "the guest's fix arrives"
    );
    assert_eq!(
        fs::read_to_string(repos.host.join("notes.md")).unwrap(),
        "written while the guest worked\n",
        "and the host's unrelated edit survives it"
    );
}

/// The tree hash the guest reports is checked against the objects actually
/// fetched, so what lands on the host is *proven* to be what the guest published
/// — not merely what it claimed. Same reasoning as the forward sync's tree-hash
/// verify, in the direction that writes to the user's own files.
#[test]
fn a_writeback_whose_tree_is_not_what_was_fetched_is_refused() {
    let repos = setup();
    let base = sync(&repos);

    write(&repos.guest, "src/main.rs", "fn main() { /* guest */ }\n");
    let mut wb = guest::tree(repos.guest.to_str().unwrap()).unwrap();
    // A tree hash that is neither the base's nor the one the guest published.
    wb.tree = "0000000000000000000000000000000000000000".to_string();

    let err = host::apply_writeback(&repos.host, repos.guest.to_str().unwrap(), &base, &wb, None)
        .expect_err("a writeback that does not match its objects must be refused");
    assert!(
        err.to_string().contains("writeback mismatch"),
        "the error has to name the cause: {err}"
    );
    assert_eq!(
        fs::read_to_string(repos.host.join("src/main.rs")).unwrap(),
        "fn main() {}\n",
        "nothing may be applied from a writeback that failed its check"
    );
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
    let snap = host::sync_to(&host_dir, "test", guest_path, None, &[]).unwrap();
    let tree = guest::apply(guest_path, &snap.commit).unwrap();
    assert_eq!(tree, snap.tree);
    assert!(guest_dir.join("hello.txt").exists());
}

// ── --with-file: forcing gitignored files into the snapshot ──────────────────

/// The `setup()` repo ignores `*.log` and `target/`; give it an ignored `.env`
/// too, which is the case the flag exists for.
fn with_dotenv(repos: &Repos) {
    write(&repos.host, ".gitignore", "target/\n*.log\n.env\n");
    write(&repos.host, ".env", "API_KEY=secret\n");
}

#[test]
fn a_gitignored_file_reaches_the_guest_only_when_forced() {
    let repos = setup();
    with_dotenv(&repos);

    sync(&repos);
    assert!(
        !repos.guest.join(".env").exists(),
        "a plain sync must leave gitignored files on the host"
    );

    sync_with(&repos, &[".env"]);
    assert_eq!(
        fs::read_to_string(repos.guest.join(".env")).unwrap(),
        "API_KEY=secret\n",
        "--with-file must carry the file's contents to the guest"
    );
}

#[test]
fn a_forced_file_leaves_when_the_flag_does() {
    // The semantics the flag promises: the file is in the guest *iff* the last
    // sync named it. Nothing lingers, so a secret cannot outlive the run that
    // asked for it — and `vm exec` without the flag is always the same run.
    let repos = setup();
    with_dotenv(&repos);
    sync_with(&repos, &[".env"]);
    assert!(repos.guest.join(".env").exists());

    sync(&repos);
    assert!(
        !repos.guest.join(".env").exists(),
        "dropping --with-file must remove the file from the guest checkout"
    );
}

#[test]
fn forcing_a_file_does_not_pollute_later_snapshots() {
    // The regression this whole design exists to prevent. `add -A` keeps paths
    // the index already tracks even when ignored — that is what carries
    // tracked-but-ignored files — so forcing `.env` *into the persistent index*
    // would silently re-add it to every later snapshot. The proof is a tree
    // hash: after a forced sync, an unforced one must be byte-for-byte the tree
    // a repo that never saw the flag produces.
    let clean = setup();
    with_dotenv(&clean);
    let never_forced = sync(&clean);

    let repos = setup();
    with_dotenv(&repos);
    sync_with(&repos, &[".env"]);
    let after_forcing = sync(&repos);

    assert_eq!(
        after_forcing.tree, never_forced.tree,
        "the persistent index must be left exactly as a plain sync would leave it"
    );
}

#[test]
fn forcing_is_deterministic() {
    // Same tree + same forced files → same commit, so a repeated `vm exec
    // --with-file .env` is the same no-op an ordinary re-sync is.
    let repos = setup();
    with_dotenv(&repos);
    assert_eq!(sync_with(&repos, &[".env"]), sync_with(&repos, &[".env"]));
}

#[test]
fn several_files_can_be_forced_at_once() {
    let repos = setup();
    with_dotenv(&repos);
    write(&repos.host, ".env.local", "DEBUG=1\n");
    write(
        &repos.host,
        ".gitignore",
        "target/\n*.log\n.env\n.env.local\n",
    );

    sync_with(&repos, &[".env", ".env.local"]);

    assert!(repos.guest.join(".env").exists());
    assert_eq!(
        fs::read_to_string(repos.guest.join(".env.local")).unwrap(),
        "DEBUG=1\n"
    );
}

#[test]
fn a_forced_file_survives_in_a_subdirectory() {
    // Paths are repo-root-relative, and a nested one must land nested.
    let repos = setup();
    write(
        &repos.host,
        ".gitignore",
        "target/\n*.log\nconfig/secrets.toml\n",
    );
    write(&repos.host, "config/secrets.toml", "token = \"abc\"\n");

    sync_with(&repos, &["config/secrets.toml"]);

    assert_eq!(
        fs::read_to_string(repos.guest.join("config/secrets.toml")).unwrap(),
        "token = \"abc\"\n"
    );
}

#[test]
fn forcing_works_in_a_repo_with_no_commits() {
    // No HEAD to seed from, and `add -A` may not have written an index file at
    // all — the copied-index path must cope with having nothing to copy.
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = tmp.path().join("fresh");
    let guest_dir = tmp.path().join("fresh-guest");
    fs::create_dir_all(&host_dir).unwrap();
    git(&host_dir, &["init", "--quiet"]);
    git(&host_dir, &["config", "user.name", "test"]);
    git(&host_dir, &["config", "user.email", "test@local"]);
    write(&host_dir, ".gitignore", ".env\n");
    write(&host_dir, ".env", "API_KEY=fresh\n");

    let guest_path = guest_dir.to_str().unwrap();
    guest::ensure_init(guest_path).unwrap();
    let snap = host::sync_to(&host_dir, "test", guest_path, None, &[".env".to_string()]).unwrap();
    let tree = guest::apply(guest_path, &snap.commit).unwrap();
    assert_eq!(tree, snap.tree);
    assert_eq!(
        fs::read_to_string(guest_dir.join(".env")).unwrap(),
        "API_KEY=fresh\n"
    );
}

#[test]
fn forcing_an_already_tracked_file_changes_nothing() {
    // `--with-file src/main.rs` is pointless but harmless: a tracked file is in
    // the snapshot either way, and the flag must not produce a different tree.
    let repos = setup();
    let plain = sync(&repos);
    let forced = sync_with(&repos, &["src/main.rs"]);
    assert_eq!(plain.tree, forced.tree);
}
