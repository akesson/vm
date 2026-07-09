use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

/// A git invocation context: repo directory, optional alternate index file,
/// optional GIT_SSH_COMMAND. The alternate index is how snapshots stay
/// completely isolated from the user's staging area — it is scoped
/// per-command here and never exported to the environment.
#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
    index: Option<PathBuf>,
    ssh_command: Option<String>,
}

impl Git {
    pub fn in_dir(cwd: impl Into<PathBuf>) -> Git {
        Git {
            cwd: cwd.into(),
            index: None,
            ssh_command: None,
        }
    }

    pub fn with_index(&self, index: PathBuf) -> Git {
        Git {
            index: Some(index),
            ..self.clone()
        }
    }

    pub fn with_ssh_command(&self, ssh: String) -> Git {
        Git {
            ssh_command: Some(ssh),
            ..self.clone()
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.cwd).args(args);
        if let Some(index) = &self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        if let Some(ssh) = &self.ssh_command {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        cmd
    }

    /// Run, discarding stdout; error carries stderr.
    pub fn run(&self, args: &[&str]) -> Result<()> {
        let out = self.command(args).output().context("failed to run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Run and return raw (untrimmed) stdout bytes — for patch content,
    /// where trailing newlines are significant.
    pub fn out_raw(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = self.command(args).output().context("failed to run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Run and return trimmed stdout.
    pub fn out(&self, args: &[&str]) -> Result<String> {
        let out = self.command(args).output().context("failed to run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
    }

    /// Absolute path of the .git directory (worktree-aware).
    pub fn git_dir(&self) -> Result<PathBuf> {
        let dir = self.out(&["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(dir))
    }

    /// HEAD commit sha, or None in a repo with no commits yet.
    pub fn head(&self) -> Option<String> {
        self.out(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Create a commit object for `tree` with a fixed author/date so that
    /// identical content yields an identical sha (idempotent re-syncs).
    pub fn commit_tree(&self, tree: &str, parent: Option<&str>) -> Result<String> {
        let mut cmd = self.command(&[]);
        cmd.args(["commit-tree", tree, "-m", "vm sync snapshot"]);
        if let Some(parent) = parent {
            cmd.args(["-p", parent]);
        }
        for prefix in ["GIT_AUTHOR", "GIT_COMMITTER"] {
            cmd.env(format!("{prefix}_NAME"), "vm-sync");
            cmd.env(format!("{prefix}_EMAIL"), "vm-sync@local");
            cmd.env(format!("{prefix}_DATE"), "2000-01-01T00:00:00+0000");
        }
        let out = cmd.output().context("failed to run git commit-tree")?;
        if !out.status.success() {
            bail!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
    }
}
