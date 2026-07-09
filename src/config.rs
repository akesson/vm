use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Machine-level VM inventory, loaded from `~/.config/vm/config.toml`
/// (override with `$VM_CONFIG`). Lives with the machine, not the repo:
/// any repo on this host can use any configured VM without per-repo setup.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub vm: BTreeMap<String, VmConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    /// Exact VM name as shown by `prlctl list -a`
    pub parallels_name: String,
    pub os: GuestOs,
    /// Guest user for ssh
    pub user: String,
    /// Directory in the guest under which per-repo checkouts live.
    /// A leading `~/` is expanded by the guest agent.
    pub work_root: String,
    /// Hostname/IP override; by default the IP is discovered via prlctl
    pub host: Option<String>,
    /// Path of the vm agent binary in the guest (default: <home>/.vm/bin/vm[.exe])
    pub agent_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    Windows,
    Linux,
    Macos,
}

impl GuestOs {
    /// The OS this `vm` process is running on, if it is one we target.
    pub fn current() -> GuestOs {
        if cfg!(target_os = "windows") {
            GuestOs::Windows
        } else if cfg!(target_os = "macos") {
            GuestOs::Macos
        } else {
            GuestOs::Linux
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("VM_CONFIG") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config/vm/config.toml")
    }

    pub fn load() -> Result<Config> {
        let path = Self::path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config at {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config at {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// Look up a VM by alias, with an error that lists what is configured.
    pub fn get(&self, alias: &str) -> Result<&VmConfig> {
        self.vm.get(alias).ok_or_else(|| {
            let known: Vec<&str> = self.vm.keys().map(String::as_str).collect();
            anyhow::anyhow!(
                "unknown VM alias '{alias}' (configured: {})",
                known.join(", ")
            )
        })
    }

    /// Find the (single) VM configured for an OS.
    pub fn find_by_os(&self, os: GuestOs) -> Result<(&str, &VmConfig)> {
        let mut matches = self.vm.iter().filter(|(_, vm)| vm.os == os);
        let Some((alias, vm)) = matches.next() else {
            bail!(
                "no VM configured for os '{os:?}' in {}",
                Self::path().display()
            );
        };
        if let Some((other, _)) = matches.next() {
            bail!(
                "multiple VMs configured for os '{os:?}' ({alias}, {other}); use `vm exec <alias>`"
            );
        }
        Ok((alias.as_str(), vm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [vm.win]
        parallels_name = "Windows 11"
        os = "windows"
        user = "henrik"
        work_root = 'C:\work'

        [vm.lin]
        parallels_name = "Ubuntu 24.04"
        os = "linux"
        user = "parallels"
        work_root = "~/work"

        [vm.mac]
        parallels_name = "macOS"
        os = "macos"
        user = "henrik"
        work_root = "~/work"
        host = "mac-vm.local"
    "#;

    #[test]
    fn parses_sample() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.vm.len(), 3);
        let win = cfg.get("win").unwrap();
        assert_eq!(win.os, GuestOs::Windows);
        assert_eq!(win.work_root, r"C:\work");
        assert_eq!(win.host, None);
        assert_eq!(
            cfg.get("mac").unwrap().host.as_deref(),
            Some("mac-vm.local")
        );
    }

    #[test]
    fn unknown_alias_lists_configured() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let err = cfg.get("bsd").unwrap_err().to_string();
        assert!(err.contains("bsd"), "{err}");
        assert!(err.contains("lin, mac, win"), "{err}");
    }

    #[test]
    fn find_by_os_picks_the_single_match() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let (alias, _) = cfg.find_by_os(GuestOs::Linux).unwrap();
        assert_eq!(alias, "lin");
    }

    #[test]
    fn find_by_os_rejects_ambiguity() {
        let two = format!(
            "{SAMPLE}\n[vm.win2]\nparallels_name = \"W2\"\nos = \"windows\"\nuser = \"u\"\nwork_root = 'C:\\w'\n"
        );
        let cfg = Config::parse(&two).unwrap();
        let err = cfg.find_by_os(GuestOs::Windows).unwrap_err().to_string();
        assert!(err.contains("multiple"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[vm.x]\nparallels_name = \"X\"\nos = \"linux\"\nuser = \"u\"\nwork_root = \"/w\"\ntypo_field = 1\n";
        assert!(Config::parse(bad).is_err());
    }
}
