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
    /// The literal OS names `exec --or-native` recognizes in a target. This is
    /// not a lookup — a target named exactly `windows`/`linux`/`macos` that
    /// matches the host OS runs natively before the config is even loaded, so
    /// the same task line works on a CI runner with no config or Parallels.
    pub fn parse(s: &str) -> Option<GuestOs> {
        match s {
            "windows" => Some(GuestOs::Windows),
            "linux" => Some(GuestOs::Linux),
            "macos" => Some(GuestOs::Macos),
            _ => None,
        }
    }

    /// The canonical OS-name string (inverse of [`GuestOs::parse`]).
    pub fn as_str(self) -> &'static str {
        match self {
            GuestOs::Windows => "windows",
            GuestOs::Linux => "linux",
            GuestOs::Macos => "macos",
        }
    }

    /// The OS this `vm` process is itself running on — the host. Used by
    /// `exec --or-native` to decide whether the target OS is already the host,
    /// so the command can skip the VM and run in place.
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
    /// The alias is the only way to address a VM — there is no OS-name
    /// fallback; the one os-literal special case lives in `exec --or-native`
    /// (see [`GuestOs::parse`]).
    ///
    /// An alias that *is* an os name but is not configured gets an extra line:
    /// that shape is a CI-portable `--or-native` task line landing on a host of
    /// a different OS, and the fix — name the VM for its OS — is not obvious
    /// from "unknown alias" alone.
    pub fn get(&self, alias: &str) -> Result<&VmConfig> {
        if let Some(vm) = self.vm.get(alias) {
            return Ok(vm);
        }
        let known: Vec<&str> = self.vm.keys().map(String::as_str).collect();
        let mut msg = format!(
            "unknown VM alias '{alias}' (configured: {})",
            known.join(", ")
        );
        if let Some(os) = GuestOs::parse(alias) {
            let same_os: Vec<&str> = self
                .vm
                .iter()
                .filter(|(_, vm)| vm.os == os)
                .map(|(a, _)| a.as_str())
                .collect();
            msg.push_str(&format!(
                "\n  '{alias}' is an OS name, but VMs are addressed by alias only. For a task \
                 line that runs natively on a {alias} host and in the VM elsewhere, the VM's \
                 alias must itself be '{alias}' — "
            ));
            match same_os.as_slice() {
                [] => msg.push_str(&format!("no VM is configured with os = \"{alias}\".")),
                aliases => msg.push_str(&format!(
                    "rename [vm.{}] to [vm.{alias}] in {}.",
                    aliases.join("] / [vm."),
                    Self::path().display()
                )),
            }
        }
        Err(usage(msg))
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
    fn os_names_are_not_aliases() {
        // Aliases are the only addressing mode: an OS name that is not a
        // configured alias is an error like any other unknown alias.
        let cfg = Config::parse(SAMPLE).unwrap();
        let err = cfg.get("windows").unwrap_err().to_string();
        assert!(err.contains("lin, mac, win"), "{err}");
    }

    #[test]
    fn an_os_named_target_points_at_the_vm_to_rename() {
        // The CI-portable `--or-native <os>` shape only works when the alias is
        // the os name, so say which VM to rename rather than just "unknown".
        let cfg = Config::parse(SAMPLE).unwrap();
        let err = cfg.get("windows").unwrap_err().to_string();
        assert!(err.contains("addressed by alias only"), "{err}");
        assert!(err.contains("rename [vm.win] to [vm.windows]"), "{err}");
    }

    #[test]
    fn an_os_with_no_vm_says_so_instead_of_naming_one() {
        let cfg = Config::parse(
            "[vm.lin]\nparallels_name = \"U\"\nos = \"linux\"\nuser = \"u\"\nwork_root = \"~/w\"\n",
        )
        .unwrap();
        let err = cfg.get("windows").unwrap_err().to_string();
        assert!(
            err.contains("no VM is configured with os = \"windows\""),
            "{err}"
        );
    }

    #[test]
    fn current_matches_the_build_target() {
        let expected = if cfg!(target_os = "windows") {
            GuestOs::Windows
        } else if cfg!(target_os = "macos") {
            GuestOs::Macos
        } else {
            GuestOs::Linux
        };
        assert_eq!(GuestOs::current(), expected);
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = "[vm.x]\nparallels_name = \"X\"\nos = \"linux\"\nuser = \"u\"\nwork_root = \"/w\"\ntypo_field = 1\n";
        assert!(Config::parse(bad).is_err());
    }
}
