use anyhow::{Context, Result, bail};
use std::borrow::Cow;
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
            return Err(crate::exit::usage(
                "not inside a git repository — run vm from within the repo you want to sync",
            ));
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

/// Guest path of the repo checkout: `<work_root>/<repo_name>`. Forward
/// slashes everywhere — Windows APIs, cargo, and Git Bash all accept them,
/// and it keeps paths uniform across guests (`C:\work` → `C:/work/repo`).
pub fn guest_repo_path(work_root: &str, repo_name: &str) -> String {
    let root = work_root.trim_end_matches(['/', '\\']).replace('\\', "/");
    format!("{root}/{repo_name}")
}

/// git remote for push/fetch against the guest checkout's repository.
/// `~/`-relative and Windows drive paths use scp-style syntax, because the
/// path arrives at the remote shell exactly as written (ssh:// URLs prefix a
/// `/` that git-receive-pack on Windows cannot resolve); home-relative paths
/// simply drop the `~/` (scp-style paths resolve relative to $HOME).
pub fn ssh_remote_url(user: &str, host: &str, guest_repo_path: &str) -> String {
    let host = url_host(host);
    let path = guest_repo_path.replace('\\', "/");
    match path {
        p if p.starts_with("~/") => format!("{user}@{host}:{}", &p[2..]),
        p if p.starts_with('/') => format!("ssh://{user}@{host}{p}"),
        p => format!("{user}@{host}:{p}"), // e.g. "C:/work/repo"
    }
}

/// A host as it must appear inside a git remote. An IPv6 literal has to be
/// bracketed: scp-style syntax splits host from path on the *first* colon, so
/// a bare `fdb2:2c26:…` reads as the host `fdb2` and ssh tries to resolve it
/// (#35). Only a literal can contain a colon — no hostname or IPv4 does.
fn url_host(host: &str) -> Cow<'_, str> {
    if host.contains(':') && !host.starts_with('[') {
        Cow::Owned(format!("[{host}]"))
    } else {
        Cow::Borrowed(host)
    }
}

/// Guest working directory for a host location inside the repo.
pub fn guest_cwd(work_root: &str, repo_name: &str, rel: &Path) -> Result<String> {
    let mut path = guest_repo_path(work_root, repo_name);
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .with_context(|| format!("non-UTF-8 path component in {}", rel.display()))?;
                path.push('/');
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
    fn windows_repo_path_normalizes_to_forward_slashes() {
        assert_eq!(guest_repo_path(r"C:\work", "syncfs"), "C:/work/syncfs");
        assert_eq!(guest_repo_path("C:\\work\\", "syncfs"), "C:/work/syncfs");
    }

    #[test]
    fn unix_repo_path_keeps_tilde_for_guest_expansion() {
        assert_eq!(guest_repo_path("~/work", "syncfs"), "~/work/syncfs");
        assert_eq!(guest_repo_path("/Users/h/work/", "vm"), "/Users/h/work/vm");
    }

    #[test]
    fn cwd_maps_subdirs_with_guest_separator() {
        let rel = Path::new("crates/syncfs-windows");
        let cwd = guest_cwd(r"C:\work", "syncfs", rel).unwrap();
        assert_eq!(cwd, "C:/work/syncfs/crates/syncfs-windows");

        let cwd = guest_cwd("~/work", "syncfs", rel).unwrap();
        assert_eq!(cwd, "~/work/syncfs/crates/syncfs-windows");
    }

    #[test]
    fn cwd_at_repo_root() {
        let cwd = guest_cwd("~/work", "syncfs", Path::new("")).unwrap();
        assert_eq!(cwd, "~/work/syncfs");
    }

    #[test]
    fn cwd_rejects_parent_components() {
        assert!(guest_cwd("~/w", "r", Path::new("../escape")).is_err());
    }

    #[test]
    fn remote_urls_for_each_path_style() {
        assert_eq!(
            ssh_remote_url("parallels", "10.211.55.4", "~/work/syncfs"),
            "parallels@10.211.55.4:work/syncfs"
        );
        assert_eq!(
            ssh_remote_url("hakesson", "10.211.55.5", r"C:\work\syncfs"),
            "hakesson@10.211.55.5:C:/work/syncfs"
        );
        assert_eq!(
            ssh_remote_url("henrik", "mac-vm.local", "/Users/henrik/work/vm"),
            "ssh://henrik@mac-vm.local/Users/henrik/work/vm"
        );
    }

    /// scp-style syntax splits host from path on the *first* colon, so a bare
    /// IPv6 literal reads as host `fdb2` and ssh tries to resolve it (#35). A
    /// guest reached over IPv6 — a configured `host`, now that `ip()` only ever
    /// returns IPv4 — must be merely unusual, not broken.
    #[test]
    fn remote_urls_bracket_ipv6_hosts() {
        assert_eq!(
            ssh_remote_url(
                "hakesson",
                "fdb2:2c26:f4e4:0:357:80a2:89e0:6574",
                "~/work/vm"
            ),
            "hakesson@[fdb2:2c26:f4e4:0:357:80a2:89e0:6574]:work/vm"
        );
        assert_eq!(
            ssh_remote_url("hakesson", "fdb2::5", r"C:\work\vm"),
            "hakesson@[fdb2::5]:C:/work/vm"
        );
        assert_eq!(
            ssh_remote_url("henrik", "fdb2::5", "/Users/henrik/work/vm"),
            "ssh://henrik@[fdb2::5]/Users/henrik/work/vm"
        );
        // An already-bracketed host is not bracketed twice.
        assert_eq!(
            ssh_remote_url("h", "[fdb2::5]", "~/work/vm"),
            "h@[fdb2::5]:work/vm"
        );
    }
}
