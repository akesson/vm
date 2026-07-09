use super::{Git, Snapshot, snapshot};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Snapshot the host repo for a given sync peer and push it to `remote_url`
/// (an ssh:// URL for real guests; any git-accepted URL/path in tests).
/// Returns the snapshot so the caller can ask the guest to apply
/// `snapshot.commit` and verify the reported tree hash matches
/// `snapshot.tree`.
pub fn sync_to(
    repo_root: &Path,
    peer: &str,
    remote_url: &str,
    ssh_command: Option<&str>,
) -> Result<Snapshot> {
    let git = Git::in_dir(repo_root);
    let snap = snapshot(&git, &format!("vm-sync-index-{peer}"))?;
    let git = match ssh_command {
        Some(ssh) => git.with_ssh_command(ssh.to_string()),
        None => git,
    };
    // Force-update: successive snapshots share a parent (host HEAD), so they
    // are never fast-forwards of each other. refs/sync/head is ours alone.
    git.run(&[
        "push",
        "--quiet",
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
