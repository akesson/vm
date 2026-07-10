//! Git-object sync: the host working tree is the source of truth.
//!
//! Host side snapshots the dirty working tree into a commit object without
//! touching the real index (a persistent alternate index file keeps the stat
//! cache warm), pushes it over ssh to the guest checkout's own repository,
//! and the guest resets to it. Verification is tree-hash equality, so a
//! successful sync is *proven*, not assumed. Writeback reuses the same
//! snapshot mechanism in the other direction.

mod git;
pub mod guest;
pub mod host;

pub use git::Git;

/// A snapshot of a working tree as unreferenced git objects.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
}

/// Snapshot the working tree of `repo` into a commit object, leaving the
/// real index (staging area) untouched. `index_name` is the file name of the
/// persistent alternate index inside `.git/` (one per sync peer, so stat
/// caches stay warm per-target).
///
/// The commit is deterministic: same tree + same parent → same sha, which
/// makes repeated syncs of an unchanged tree no-ops end to end.
///
/// Not internally synchronized: the alternate index's `.lock` tolerates no
/// concurrent writer. Host-side callers serialize per (repo, peer) via
/// [`host::lock_sync`]; the guest side has a single writer by construction.
pub fn snapshot(git: &Git, index_name: &str) -> anyhow::Result<Snapshot> {
    let index = git.git_dir()?.join(index_name);
    let g = git.with_index(index.clone());

    // Seed from HEAD once: a fresh `add -A` index would silently drop files
    // that are tracked but also match .gitignore.
    let head = g.head();
    if !index.exists()
        && let Some(head) = &head
    {
        g.run(&["read-tree", head])?;
    }
    g.run(&["add", "-A"])?;
    let tree = g.out(&["write-tree"])?;

    let commit = match &head {
        Some(head) => g.commit_tree(&tree, Some(head))?,
        None => g.commit_tree(&tree, None)?,
    };
    Ok(Snapshot { commit, tree })
}

/// Expand a leading `~/` against the platform home directory. Guest-side:
/// config `work_root` may be `~/work` so one config works for any user name.
pub fn expand_home(path: &str) -> anyhow::Result<std::path::PathBuf> {
    if let Some(rest) = path.strip_prefix("~/").or(path.strip_prefix("~\\")) {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| anyhow::anyhow!("cannot expand '~': no HOME or USERPROFILE set"))?;
        return Ok(std::path::PathBuf::from(home).join(rest));
    }
    Ok(std::path::PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_leaves_absolute_paths() {
        assert_eq!(
            expand_home("/abs/path").unwrap(),
            std::path::PathBuf::from("/abs/path")
        );
        assert_eq!(
            expand_home(r"C:\work").unwrap(),
            std::path::PathBuf::from(r"C:\work")
        );
    }

    #[test]
    fn expand_home_expands_tilde() {
        let p = expand_home("~/work/repo").unwrap();
        let p = p.to_string_lossy().replace('\\', "/");
        // On Windows the home prefix uses `\` but the joined tail keeps `/`;
        // compare separator-normalized.
        assert!(!p.contains('~'));
        assert!(p.ends_with("work/repo"));
    }
}
