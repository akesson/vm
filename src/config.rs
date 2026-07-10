use crate::exit::usage;
use anyhow::Result;
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
    #[allow(dead_code)] // used from phase 4 (deploy/exec)
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
    /// The OS names accepted as exec targets.
    pub fn parse(s: &str) -> Option<GuestOs> {
        match s {
            "windows" => Some(GuestOs::Windows),
            "linux" => Some(GuestOs::Linux),
            "macos" => Some(GuestOs::Macos),
            _ => None,
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
        let text = std::fs::read_to_string(&path).map_err(|e| {
            usage(format!(
                "cannot read config at {} ({e}) — create it, or point $VM_CONFIG at one",
                path.display()
            ))
        })?;
        Self::parse(&text)
            .map_err(|e| usage(format!("invalid config at {} — {e:#}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// Look up a VM by alias, with an error that lists what is configured.
    pub fn get(&self, alias: &str) -> Result<&VmConfig> {
        self.vm.get(alias).ok_or_else(|| {
            let known: Vec<&str> = self.vm.keys().map(String::as_str).collect();
            usage(format!(
                "unknown VM alias '{alias}' (configured: {})",
                known.join(", ")
            ))
        })
    }

    /// Resolve an exec target: an alias, or an OS name (`windows` | `linux` |
    /// `macos`) selecting the single VM configured for that OS. Aliases win
    /// on collision. Never resolves to the host — `vm` always targets a VM.
    pub fn resolve(&self, target: &str) -> Result<(&str, &VmConfig)> {
        if let Some((alias, vm)) = self.vm.get_key_value(target) {
            return Ok((alias.as_str(), vm));
        }
        if let Some(os) = GuestOs::parse(target) {
            return self.find_by_os(os);
        }
        let known: Vec<&str> = self.vm.keys().map(String::as_str).collect();
        Err(usage(format!(
            "unknown target '{target}' — expected a configured alias ({}) or an OS name (windows, linux, macos)",
            known.join(", ")
        )))
    }

    /// Find the (single) VM configured for an OS.
    pub fn find_by_os(&self, os: GuestOs) -> Result<(&str, &VmConfig)> {
        let mut matches = self.vm.iter().filter(|(_, vm)| vm.os == os);
        let Some((alias, vm)) = matches.next() else {
            return Err(usage(format!(
                "no VM configured for os '{os:?}' in {}",
                Self::path().display()
            )));
        };
        if let Some((other, _)) = matches.next() {
            return Err(usage(format!(
                "multiple VMs configured for os '{os:?}' ({alias}, {other}); use `vm exec <alias>`"
            )));
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
    fn resolve_prefers_alias_then_falls_back_to_os_name() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.resolve("win").unwrap().0, "win");
        assert_eq!(cfg.resolve("windows").unwrap().0, "win");
        assert_eq!(cfg.resolve("macos").unwrap().0, "mac");
    }

    #[test]
    fn resolve_rejects_unknown_target_mentioning_both_forms() {
        let cfg = Config::parse(SAMPLE).unwrap();
        let err = cfg.resolve("ios").unwrap_err().to_string();
        assert!(err.contains("lin, mac, win"), "{err}");
        assert!(err.contains("windows, linux, macos"), "{err}");
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
