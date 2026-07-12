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
/// `extra` holds repo-root-relative paths to force into the snapshot even when
/// gitignored — the `--with-file` escape hatch, for the `.env` a build needs
/// and git refuses to see. Empty for a plain sync.
///
/// The commit is deterministic: same tree + same parent → same sha, which
/// makes repeated syncs of an unchanged tree no-ops end to end. That holds with
/// `extra` too: same files forced, same tree.
///
/// Not internally synchronized: the alternate index's `.lock` tolerates no
/// concurrent writer. Host-side callers serialize per (repo, peer) via
/// [`host::lock_sync`]; the guest side has a single writer by construction.
pub fn snapshot(git: &Git, index_name: &str, extra: &[String]) -> anyhow::Result<Snapshot> {
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
    let tree = match extra {
        [] => g.out(&["write-tree"])?,
        extra => tree_with_forced(git, &index, index_name, extra)?,
    };

    let commit = match &head {
        Some(head) => g.commit_tree(&tree, Some(head))?,
        None => g.commit_tree(&tree, None)?,
    };
    Ok(Snapshot { commit, tree })
}

/// The tree of the working tree *plus* `extra`, written through a throwaway
/// **copy** of the persistent index — never the index itself.
///
/// That indirection is the whole point. `add -A` keeps every path the index
/// already tracks, ignored or not: it is exactly what the HEAD seed in
/// [`snapshot`] leans on to hold tracked-but-ignored files. So a `.env` forced
/// into the *persistent* index would still be there on the next sync, and the
/// one after that — silently riding along long after the caller dropped
/// `--with-file`, with no way to get it back out. Forced files must live for
/// exactly one snapshot, so they are written somewhere that only lives that
/// long.
///
/// The copy inherits the stat cache the persistent index just warmed, and is
/// overwritten from it on every use, so a previous run's forced files are gone
/// before this one starts.
fn tree_with_forced(
    git: &Git,
    index: &std::path::Path,
    index_name: &str,
    extra: &[String],
) -> anyhow::Result<String> {
    use anyhow::Context;
    let scratch = git.git_dir()?.join(format!("{index_name}-forced"));
    match std::fs::copy(index, &scratch) {
        Ok(_) => {}
        // No index file to copy: `add -A` writes none in a repo that has nothing
        // to add yet (a fresh `git init`). Start from empty — and from *nothing*,
        // not from whatever a previous run left in the scratch file.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if scratch.exists() {
                std::fs::remove_file(&scratch)
                    .with_context(|| format!("removing stale {}", scratch.display()))?;
            }
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("copying sync index to {}", scratch.display()));
        }
    }
    let g = git.with_index(scratch);
    // `--` so a path can never be read as a revision or an option.
    let mut args = vec!["add", "-f", "--"];
    args.extend(extra.iter().map(String::as_str));
    g.run(&args)?;
    g.out(&["write-tree"])
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
