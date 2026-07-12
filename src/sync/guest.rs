use super::{Git, Snapshot, expand_home, snapshot};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// Guest half of a sync: make the checkout at `repo` (creating it on first
/// use) match the previously-pushed commit `sha` exactly, and return the
/// resulting tree hash for the host to verify.
///
/// The checkout keeps ignored files (`clean -fd`, not `-fdx`), so guest-local
/// build state like `target/` survives every sync.
pub fn apply(repo: &str, sha: &str) -> Result<String> {
    ensure_init(repo)?;
    let path = expand_home(repo)?;
    let git = Git::in_dir(&path);
    // The commit was pushed to refs/sync/head before this verb runs.
    git.run(&["reset", "--hard", sha, "--quiet"])
        .with_context(|| format!("commit {sha} not present; was the push successful?"))?;
    git.run(&["clean", "-fd", "--quiet"])?;

    // Prove the working tree matches: no tracked modifications, no strays.
    let dirt = git.out(&["status", "--porcelain"])?;
    if !dirt.is_empty() {
        bail!("checkout is not clean after sync-apply (locked files?):\n{dirt}");
    }
    git.out(&["rev-parse", &format!("{sha}^{{tree}}")])
}

/// Snapshot the guest checkout for writeback and publish it at
/// refs/sync/writeback so the host can fetch it. Prints commit + tree.
pub fn tree(repo: &str) -> Result<Snapshot> {
    let path = expand_home(repo)?;
    let git = Git::in_dir(&path);
    // Nothing to force here: a `--with-file` file arrived as an ordinary tracked
    // file of the sync commit, so the HEAD seed already snapshots it — and since
    // it sits in the writeback *base* too, it yields no diff unless the guest
    // actually edited it, which is exactly what writeback is for. Forcing is a
    // host-side decision about the host's gitignore; the guest never makes it.
    let snap = snapshot(&git, "vm-writeback-index", &[])?;
    git.run(&["update-ref", "refs/sync/writeback", &snap.commit])?;
    Ok(snap)
}

/// Create and configure the checkout repository if it does not exist yet.
/// Idempotent; called before the host pushes objects for the first time.
pub fn ensure_init(repo: &str) -> Result<()> {
    let path = expand_home(repo)?;
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create checkout dir {}", path.display()))?;
    if !path.join(".git").exists() {
        init(&Git::in_dir(&path), &path)?;
    }
    Ok(())
}

fn init(git: &Git, path: &Path) -> Result<()> {
    git.run(&["init", "--quiet"])?;
    // Byte-identical trees on every OS, or tree-hash verification would lie.
    git.run(&["config", "core.autocrlf", "false"])?;
    git.run(&["config", "core.filemode", "false"])?;
    // Guest checkouts are disposable replicas; identity only matters for
    // writeback snapshot objects, which carry their own fixed identity.
    git.run(&["config", "user.name", "vm-sync"])?;
    git.run(&["config", "user.email", "vm-sync@local"])?;
    eprintln!("vm ▸ initialized checkout at {}", path.display());
    Ok(())
}
