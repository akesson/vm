//! Guest environment managers (`--guest-env`).
//!
//! A guest env is a dev-environment tool vm knows how to set up in the guest
//! checkout, replacing the old per-repo `.vm.toml` (`on_first_sync` + `wrap`)
//! with two fixed, tool-specific behaviors: a one-time setup command run when
//! a checkout is first created, and an argv prefix wrapped around exec'd
//! commands so the checkout's tools resolve. Detected from marker files at the
//! host repo root; detection is never silent — an active env is announced with
//! a breadcrumb, and `--guest-env` forces or disables it per invocation.

use crate::crumb;
use std::path::Path;

/// The `--guest-env` choices. An absent flag means auto-detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GuestEnv {
    /// mise-managed repo: first sync runs `mise trust`; guest commands are
    /// wrapped with `mise exec --`.
    Mise,
    /// No guest-env handling, even when detection would find one.
    None,
}

/// Repo-root files that mark a mise-managed repo, most common first.
const MISE_MARKERS: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    ".mise/config.toml",
    ".config/mise/config.toml",
];

/// The guest env in effect for one run, plus where the decision came from so
/// the breadcrumb can say why.
pub struct ActiveEnv {
    pub env: GuestEnv,
    /// `None` for an explicit `--guest-env`, else the detected marker file.
    detected_from: Option<&'static str>,
}

/// Decide the guest env: an explicit `--guest-env` wins; otherwise detect
/// from the host repo root.
pub fn resolve(flag: Option<GuestEnv>, repo_root: &Path) -> ActiveEnv {
    if let Some(env) = flag {
        return ActiveEnv {
            env,
            detected_from: None,
        };
    }
    match MISE_MARKERS.iter().find(|f| repo_root.join(f).is_file()) {
        Some(marker) => ActiveEnv {
            env: GuestEnv::Mise,
            detected_from: Some(marker),
        },
        None => ActiveEnv {
            env: GuestEnv::None,
            detected_from: None,
        },
    }
}

impl ActiveEnv {
    /// Announce an active env on stderr. Detection must never be implicit:
    /// whoever reads the run log sees what will be wrapped and set up, and how
    /// to turn it off.
    pub fn announce(&self, alias: &str) {
        if self.env == GuestEnv::None {
            return;
        }
        match self.detected_from {
            Some(marker) => crumb!(
                "vm ▸ {alias} ▸ guest env: mise (detected {marker}) — `mise trust` on first \
                 sync, exec commands wrapped `mise exec --`; --guest-env none disables"
            ),
            None => crumb!("vm ▸ {alias} ▸ guest env: mise (--guest-env)"),
        }
    }

    /// Argv prefix prepended to guest exec commands (argv space, so it is
    /// quoting-safe; the elements ride to the guest as JSON).
    pub fn wrap(&self) -> &'static [&'static str] {
        match self.env {
            GuestEnv::Mise => &["mise", "exec", "--"],
            GuestEnv::None => &[],
        }
    }

    /// One-time setup command for a freshly created guest checkout.
    pub fn first_sync_cmd(&self) -> Option<&'static str> {
        match self.env {
            GuestEnv::Mise => Some("mise trust"),
            GuestEnv::None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_repo_detects_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let active = resolve(None, tmp.path());
        assert_eq!(active.env, GuestEnv::None);
        assert!(active.wrap().is_empty());
        assert_eq!(active.first_sync_cmd(), None);
    }

    #[test]
    fn mise_toml_at_the_root_selects_mise() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mise.toml"), "[tools]\n").unwrap();
        let active = resolve(None, tmp.path());
        assert_eq!(active.env, GuestEnv::Mise);
        assert_eq!(active.wrap(), ["mise", "exec", "--"]);
        assert_eq!(active.first_sync_cmd(), Some("mise trust"));
        assert_eq!(active.detected_from, Some("mise.toml"));
    }

    #[test]
    fn nested_mise_config_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".config/mise")).unwrap();
        std::fs::write(tmp.path().join(".config/mise/config.toml"), "").unwrap();
        assert_eq!(resolve(None, tmp.path()).env, GuestEnv::Mise);
    }

    #[test]
    fn explicit_none_overrides_detection() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mise.toml"), "").unwrap();
        let active = resolve(Some(GuestEnv::None), tmp.path());
        assert_eq!(active.env, GuestEnv::None);
        assert!(active.wrap().is_empty());
        assert_eq!(active.first_sync_cmd(), None);
    }

    #[test]
    fn explicit_mise_needs_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let active = resolve(Some(GuestEnv::Mise), tmp.path());
        assert_eq!(active.env, GuestEnv::Mise);
        assert_eq!(active.detected_from, None);
        assert_eq!(active.first_sync_cmd(), Some("mise trust"));
    }

    #[test]
    fn a_mise_dir_alone_is_not_a_marker() {
        // Only config *files* count; an empty directory must not activate mise.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mise")).unwrap();
        assert_eq!(resolve(None, tmp.path()).env, GuestEnv::None);
    }
}
