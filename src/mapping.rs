use crate::config::GuestOs;
use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Host-side repo location: root of the git worktree the user invoked `vm` from,
/// plus where the current dir sits inside it (commands run in the corresponding
/// guest directory).
pub struct RepoLocation {
    #[allow(dead_code)] // used from phase 3 (sync)
    pub root: PathBuf,
    pub name: String,
    /// Current dir relative to the repo root ("" when at the root)
    #[allow(dead_code)] // used from phase 4 (exec cwd)
    pub rel: PathBuf,
}

impl RepoLocation {
    pub fn discover() -> Result<RepoLocation> {
        let out = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            bail!("not inside a git repository (vm syncs the enclosing git repo)");
        }
        let root = PathBuf::from(String::from_utf8(out.stdout)?.trim_end());
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .context("repo root has no valid name")?
            .to_string();
        let cwd = std::env::current_dir()?;
        let rel = cwd
            .strip_prefix(&root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        Ok(RepoLocation { root, name, rel })
    }
}

fn sep(os: GuestOs) -> char {
    match os {
        GuestOs::Windows => '\\',
        GuestOs::Linux | GuestOs::Macos => '/',
    }
}

/// Guest path of the repo checkout: `<work_root><sep><repo_name>`.
pub fn guest_repo_path(os: GuestOs, work_root: &str, repo_name: &str) -> String {
    let root = work_root.trim_end_matches(['/', '\\']);
    format!("{root}{}{repo_name}", sep(os))
}

/// ssh:// URL for git push/fetch against the guest checkout's repository.
/// `~/`-relative paths use git's `/~/` form (resolved by the remote shell);
/// Windows drive paths become `/C:/…` with forward slashes.
pub fn ssh_remote_url(user: &str, host: &str, guest_repo_path: &str) -> String {
    let path = guest_repo_path.replace('\\', "/");
    let path = match path {
        p if p.starts_with("~/") => format!("/~/{}", &p[2..]),
        p if p.starts_with('/') => p,
        p => format!("/{p}"), // e.g. "C:/work/repo"
    };
    format!("ssh://{user}@{host}{path}")
}

/// Guest working directory for a host location inside the repo.
#[allow(dead_code)] // used from phase 4 (exec)
pub fn guest_cwd(os: GuestOs, work_root: &str, repo_name: &str, rel: &Path) -> Result<String> {
    let mut path = guest_repo_path(os, work_root, repo_name);
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .with_context(|| format!("non-UTF-8 path component in {}", rel.display()))?;
                path.push(sep(os));
                path.push_str(part);
            }
            Component::CurDir => {}
            other => bail!(
                "unexpected path component {other:?} in relative path {}",
                rel.display()
            ),
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_repo_path() {
        assert_eq!(
            guest_repo_path(GuestOs::Windows, r"C:\work", "syncfs"),
            r"C:\work\syncfs"
        );
        assert_eq!(
            guest_repo_path(GuestOs::Windows, "C:\\work\\", "syncfs"),
            r"C:\work\syncfs"
        );
    }

    #[test]
    fn unix_repo_path_keeps_tilde_for_guest_expansion() {
        assert_eq!(
            guest_repo_path(GuestOs::Linux, "~/work", "syncfs"),
            "~/work/syncfs"
        );
        assert_eq!(
            guest_repo_path(GuestOs::Macos, "/Users/h/work/", "vm"),
            "/Users/h/work/vm"
        );
    }

    #[test]
    fn cwd_maps_subdirs_with_guest_separator() {
        let rel = Path::new("crates/syncfs-windows");
        let cwd = guest_cwd(GuestOs::Windows, r"C:\work", "syncfs", rel).unwrap();
        assert_eq!(cwd, r"C:\work\syncfs\crates\syncfs-windows");

        let cwd = guest_cwd(GuestOs::Linux, "~/work", "syncfs", rel).unwrap();
        assert_eq!(cwd, "~/work/syncfs/crates/syncfs-windows");
    }

    #[test]
    fn cwd_at_repo_root() {
        let cwd = guest_cwd(GuestOs::Linux, "~/work", "syncfs", Path::new("")).unwrap();
        assert_eq!(cwd, "~/work/syncfs");
    }

    #[test]
    fn cwd_rejects_parent_components() {
        assert!(guest_cwd(GuestOs::Linux, "~/w", "r", Path::new("../escape")).is_err());
    }

    #[test]
    fn remote_urls_for_each_path_style() {
        assert_eq!(
            ssh_remote_url("parallels", "10.211.55.4", "~/work/syncfs"),
            "ssh://parallels@10.211.55.4/~/work/syncfs"
        );
        assert_eq!(
            ssh_remote_url("hakesson", "10.211.55.5", r"C:\work\syncfs"),
            "ssh://hakesson@10.211.55.5/C:/work/syncfs"
        );
        assert_eq!(
            ssh_remote_url("henrik", "mac-vm.local", "/Users/henrik/work/vm"),
            "ssh://henrik@mac-vm.local/Users/henrik/work/vm"
        );
    }
}
