//! Per-repo, committed config at `<repo-root>/.vm.toml`.
//!
//! Unlike the machine config ([`crate::config`], which lives with the *machine*
//! so any repo can use any configured VM), this lives with the *repo*: a
//! checkout declares the one-time setup its guest copy needs (`mise trust`,
//! `git lfs install`, …) so a teammate's first `vm exec`/`vm sync` just works.
//! A missing file is not an error — it simply means no per-repo config.

use crate::exit::usage;
use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Command run in the guest checkout the first time it is created — and
    /// again whenever the checkout is recreated (`vm clean`, a rebuilt guest, a
    /// manual delete). Runs before the exec'd command; a nonzero exit fails the
    /// run. See issue #6.
    pub on_first_sync: Option<String>,

    /// Prefix prepended to every guest `vm exec` / `vm with-snapshot` command
    /// (e.g. `["mise", "exec", "--"]` so a mise-managed guest checkout resolves
    /// its tools). Applied on the guest path only — native `--or-native` runs
    /// already have the launching environment. Prepended in argv space, so it
    /// is quoting-safe. See issue #9.
    #[serde(default)]
    pub wrap: Vec<String>,
}

impl RepoConfig {
    /// Load `<repo_root>/.vm.toml`. A missing file yields the default (empty)
    /// config. A present-but-unreadable or malformed file is a usage error
    /// ("fix your `.vm.toml`"), mirroring [`crate::config::Config::load`].
    pub fn load(repo_root: &Path) -> Result<RepoConfig> {
        let path = repo_root.join(".vm.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RepoConfig::default()),
            Err(e) => return Err(usage(format!("cannot read {} ({e})", path.display()))),
        };
        Self::parse(&text).map_err(|e| usage(format!("invalid {} — {e:#}", path.display())))
    }

    fn parse(text: &str) -> Result<RepoConfig> {
        Ok(toml::from_str(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_on_first_sync() {
        let cfg = RepoConfig::parse("on_first_sync = \"mise trust\"").unwrap();
        assert_eq!(cfg.on_first_sync.as_deref(), Some("mise trust"));
    }

    #[test]
    fn empty_file_has_no_hook() {
        assert_eq!(RepoConfig::parse("").unwrap().on_first_sync, None);
    }

    #[test]
    fn wrap_defaults_to_empty() {
        assert!(RepoConfig::parse("").unwrap().wrap.is_empty());
        assert!(
            RepoConfig::parse("on_first_sync = \"mise trust\"")
                .unwrap()
                .wrap
                .is_empty()
        );
    }

    #[test]
    fn parses_wrap_list() {
        let cfg = RepoConfig::parse("wrap = [\"mise\", \"exec\", \"--\"]").unwrap();
        assert_eq!(cfg.wrap, ["mise", "exec", "--"]);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(RepoConfig::parse("typo = 1").is_err());
    }

    #[test]
    fn missing_file_is_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.on_first_sync, None);
    }

    #[test]
    fn loads_from_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".vm.toml"),
            "on_first_sync = 'git lfs install'",
        )
        .unwrap();
        let cfg = RepoConfig::load(tmp.path()).unwrap();
        assert_eq!(cfg.on_first_sync.as_deref(), Some("git lfs install"));
    }
}
