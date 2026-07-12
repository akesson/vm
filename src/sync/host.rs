use super::{Git, Snapshot, snapshot};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Serialize the whole sync/writeback critical section for one (host repo,
/// peer). Concurrent `vm` invocations against the same guest — a `mise`
/// fan-out running two `vm exec <guest> …` in parallel, say — otherwise race
/// on three pieces of shared git state that each assume a single writer: the
/// host-side alternate index `.git/vm-sync-index-<peer>` (its `.lock`), the
/// `+…:refs/sync/head` force-push (a remote ref compare-and-swap), and the
/// guest checkout the push is then reset onto.
///
/// The per-VM lock in [`crate::lock`] is *shared* and does not cover this: it
/// only keeps stop/reap out. Holders take this after that shared lock (fixed
/// order: VM lock → sync lock), so there is no deadlock cycle.
///
/// The lock file sits in the host repo's git dir next to the index it guards
/// (worktree-aware via `--absolute-git-dir`); the `.flock` suffix keeps it
/// distinct from git's own transient `*.lock` files. It is never deleted (see
/// [`crate::lock::PathLock`]).
pub fn lock_sync(repo_root: &Path, peer: &str) -> Result<crate::lock::PathLock> {
    let path = Git::in_dir(repo_root)
        .git_dir()?
        .join(format!("vm-sync-{peer}.flock"));
    crate::lock::exclusive_path(&path, || {
        eprintln!("vm ▸ {peer} ▸ waiting for a concurrent sync of this repo…");
    })
}

/// Snapshot the host repo for a given sync peer and push it to `remote_url`
/// (an ssh:// URL for real guests; any git-accepted URL/path in tests).
/// Returns the snapshot so the caller can ask the guest to apply
/// `snapshot.commit` and verify the reported tree hash matches
/// `snapshot.tree`.
///
/// `extra` (repo-root-relative) forces gitignored paths into the snapshot —
/// `--with-file`. They ride the ordinary object push, so the tree-hash check
/// covers them like any other file: a forced `.env` is *proven* to have landed.
pub fn sync_to(
    repo_root: &Path,
    peer: &str,
    remote_url: &str,
    ssh_command: Option<&str>,
    extra: &[String],
) -> Result<Snapshot> {
    let git = Git::in_dir(repo_root);
    let snap = snapshot(&git, &format!("vm-sync-index-{peer}"), extra)?;
    let git = match ssh_command {
        Some(ssh) => git.with_ssh_command(ssh.to_string()),
        None => git,
    };
    // Force-update: successive snapshots share a parent (host HEAD), so they
    // are never fast-forwards of each other. refs/sync/head is ours alone.
    // --no-verify: this push is replication plumbing, not publishing — the
    // repo's pre-push hook (test suites, lints…) must not run, and via
    // exec wrappers it could even recurse back into `vm` itself.
    git.run(&[
        "push",
        "--quiet",
        "--no-verify",
        remote_url,
        &format!("+{}:refs/sync/head", snap.commit),
    ])
    .with_context(|| format!("pushing sync snapshot to {remote_url}"))?;
    Ok(snap)
}

/// Fetch a writeback snapshot (published by the guest at refs/sync/writeback)
/// and apply the difference `base..writeback` to the host working tree.
/// Returns true if anything was applied.
pub fn apply_writeback(
    repo_root: &Path,
    remote_url: &str,
    base: &Snapshot,
    guest_writeback: &Snapshot,
    ssh_command: Option<&str>,
) -> Result<bool> {
    if guest_writeback.tree == base.tree {
        return Ok(false);
    }
    let git = Git::in_dir(repo_root);
    let fetch_git = match ssh_command {
        Some(ssh) => git.with_ssh_command(ssh.to_string()),
        None => git.clone(),
    };
    fetch_git
        .run(&["fetch", "--quiet", remote_url, "refs/sync/writeback"])
        .with_context(|| format!("fetching writeback from {remote_url}"))?;

    let fetched_tree = git.out(&["rev-parse", "FETCH_HEAD^{tree}"])?;
    if fetched_tree != guest_writeback.tree {
        bail!(
            "writeback mismatch: guest reported tree {} but fetched {}",
            guest_writeback.tree,
            fetched_tree
        );
    }
    let patch = git.out_raw(&["diff", &base.commit, "FETCH_HEAD"])?;
    if patch.is_empty() {
        return Ok(false);
    }
    // `git apply` from stdin, against the working tree only.
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["apply", "--whitespace=nowarn"])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to spawn git apply")?;
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(&patch)?;
    drop(child.stdin.take());
    let status = child.wait()?;
    if !status.success() {
        bail!("git apply of guest writeback failed (host tree changed during the run?)");
    }
    Ok(true)
}
