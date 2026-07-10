use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// One VM as reported by `prlctl list -a --json`.
#[derive(Debug, Deserialize)]
pub struct PrlVm {
    pub uuid: String,
    pub status: String,
    /// "-" when the VM has no IP (stopped, or tools not up yet)
    pub ip_configured: String,
    pub name: String,
}

impl PrlVm {
    /// The guest's usable IP, or None while it has none yet. A waking VM
    /// briefly reports only a link-local IPv6 (fe80::…) which isn't routable
    /// without a zone id — treat that the same as "no IP yet" and keep
    /// waiting for the DHCP address.
    pub fn ip(&self) -> Option<&str> {
        let ip = self.ip_configured.as_str();
        (ip != "-" && !ip.starts_with("fe80:")).then_some(ip)
    }
}

fn prlctl(args: &[&str]) -> Result<String> {
    let out = Command::new("prlctl")
        .args(args)
        .output()
        .context("failed to run prlctl (is Parallels installed?)")?;
    if !out.status.success() {
        bail!(
            "prlctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub fn list_all() -> Result<Vec<PrlVm>> {
    // -f (full) is what makes ip_configured carry the real IP.
    let json = prlctl(&["list", "-a", "-f", "--json"])?;
    serde_json::from_str(&json).context("unexpected `prlctl list --json` output")
}

pub fn find(name: &str) -> Result<PrlVm> {
    list_all()?
        .into_iter()
        .find(|vm| vm.name == name)
        .ok_or_else(|| anyhow::anyhow!("no Parallels VM named '{name}' (see `prlctl list -a`)"))
}

/// Start or resume as appropriate; no-op when already running.
pub fn ensure_running(name: &str) -> Result<()> {
    let vm = find(name)?;
    match vm.status.as_str() {
        "running" => Ok(()),
        "stopped" => {
            eprintln!("vm ▸ starting '{name}'…");
            prlctl(&["start", name]).map(drop)
        }
        "suspended" | "paused" => {
            eprintln!("vm ▸ resuming '{name}'…");
            prlctl(&["resume", name]).map(drop)
        }
        other => bail!("VM '{name}' is in unexpected state '{other}'"),
    }
}

/// Wait until the guest reports an IP (Parallels Tools up / DHCP done).
pub fn wait_for_ip(name: &str, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(ip) = find(name)?.ip() {
            return Ok(ip.to_string());
        }
        if Instant::now() >= deadline {
            bail!(
                "VM '{name}' did not report an IP within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Base invocation for running a command in the guest's *console session*
/// (the interactive desktop) via Parallels Tools, as the console-logged-in
/// user. This is how Windows exec reaches session 1: ssh children land in
/// session 0 on a non-interactive window station, where UIA and every other
/// GUI API see an empty desktop. Caveats: argv is re-joined guest-side (no
/// POSIX shell, so `~` never expands), and it requires a user logged in at
/// the console.
pub fn exec_console(name: &str) -> Command {
    let mut cmd = Command::new("prlctl");
    cmd.args(["exec", name, "--current-user"]);
    cmd
}

/// Run a command in the console session, capturing output (for doctor).
pub fn exec_console_capture(name: &str, args: &[&str]) -> Result<Output> {
    exec_console(name)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("failed to run prlctl exec")
}

pub fn stop(name: &str, kill: bool) -> Result<()> {
    let args: &[&str] = if kill {
        &["stop", name, "--kill"]
    } else {
        &["stop", name]
    };
    prlctl(args).map(drop)
}

/// Suspend (RAM image to disk; `ensure_running` resumes in ~1s). Reap
/// plumbing only — there is deliberately no `vm suspend` command.
pub fn suspend(name: &str) -> Result<()> {
    prlctl(&["suspend", name]).map(drop)
}

/// Existing snapshots as (id, name) pairs.
pub fn snapshot_list(name: &str) -> Result<Vec<(String, String)>> {
    let json = prlctl(&["snapshot-list", name, "--json"])?;
    parse_snapshot_list(&json)
        .with_context(|| format!("unexpected `prlctl snapshot-list {name} --json` output"))
}

fn parse_snapshot_list(json: &str) -> Result<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Snap {
        name: String,
    }
    if json.trim().is_empty() {
        return Ok(vec![]); // no snapshots → empty output, not `{}`
    }
    let map: std::collections::BTreeMap<String, Snap> = serde_json::from_str(json)?;
    Ok(map.into_iter().map(|(id, s)| (id, s.name)).collect())
}

/// The subset of `prlctl list -i --json` a snapshot pre-check needs: where
/// the VM lives on the host disk, and its RAM size (a running-VM snapshot
/// writes a memory image of about that size, then grows a delta disk).
#[derive(Debug)]
pub struct VmDetails {
    pub home: String,
    pub memory_mb: u64,
}

pub fn details(name: &str) -> Result<VmDetails> {
    let json = prlctl(&["list", "-i", name, "--json"])?;
    parse_details(&json)
        .with_context(|| format!("unexpected `prlctl list -i {name} --json` output"))
}

fn parse_details(json: &str) -> Result<VmDetails> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(rename = "Home")]
        home: String,
        #[serde(rename = "Hardware")]
        hardware: Hardware,
    }
    #[derive(Deserialize)]
    struct Hardware {
        memory: Memory,
    }
    #[derive(Deserialize)]
    struct Memory {
        /// e.g. "20480Mb"
        size: String,
    }
    let mut infos: Vec<Info> = serde_json::from_str(json)?;
    let info = infos
        .pop()
        .ok_or_else(|| anyhow::anyhow!("empty VM info list"))?;
    let mb = info
        .hardware
        .memory
        .size
        .trim_end_matches("Mb")
        .parse::<u64>()
        .with_context(|| format!("cannot parse memory size '{}'", info.hardware.memory.size))?;
    Ok(VmDetails {
        home: info.home,
        memory_mb: mb,
    })
}

/// Screenshot the VM display to a PNG file.
pub fn capture(name: &str, file: &str) -> Result<()> {
    prlctl(&["capture", name, "--file", file]).map(drop)
}

/// Create a snapshot and return its id (a `{uuid}` string).
pub fn snapshot_create(name: &str, snap_name: &str) -> Result<String> {
    let out = prlctl(&["snapshot", name, "--name", snap_name])?;
    parse_snapshot_id(&out)
        .ok_or_else(|| anyhow::anyhow!("could not find a snapshot id in prlctl output: {out}"))
}

/// Roll the VM back to a snapshot (restores disk AND run state).
pub fn snapshot_switch(name: &str, id: &str) -> Result<()> {
    prlctl(&["snapshot-switch", name, "--id", id]).map(drop)
}

pub fn snapshot_delete(name: &str, id: &str) -> Result<()> {
    prlctl(&["snapshot-delete", name, "--id", id]).map(drop)
}

/// prlctl prints e.g. `The snapshot with id {8b171e2f-…} has been successfully
/// created.` — pull out the braced id.
fn parse_snapshot_id(out: &str) -> Option<String> {
    let start = out.find('{')?;
    let end = out[start..].find('}')? + start;
    Some(out[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prlctl_list_json() {
        let json = r#"[
            {"uuid": "{db670d16}", "status": "suspended", "ip_configured": "-", "name": "Ubuntu 24.04"},
            {"uuid": "{d2b7786c}", "status": "running", "ip_configured": "10.211.55.4", "name": "Windows 11"}
        ]"#;
        let vms: Vec<PrlVm> = serde_json::from_str(json).unwrap();
        assert_eq!(vms.len(), 2);
        assert_eq!(vms[0].ip(), None);
        assert_eq!(vms[1].ip(), Some("10.211.55.4"));
    }

    #[test]
    fn extracts_snapshot_id_from_prlctl_output() {
        let out = "Creating the snapshot...\nThe snapshot with id {8b171e2f-4b7f-4e01-a689-a2d360d63e49} has been successfully created.\n";
        assert_eq!(
            parse_snapshot_id(out).as_deref(),
            Some("{8b171e2f-4b7f-4e01-a689-a2d360d63e49}")
        );
        assert_eq!(parse_snapshot_id("no id here"), None);
    }

    #[test]
    fn parses_snapshot_list_including_empty() {
        assert_eq!(parse_snapshot_list("").unwrap(), vec![]);
        assert_eq!(parse_snapshot_list("\n").unwrap(), vec![]);
        // Real shape from Parallels 26.4.
        let json = r#"{
            "{351b744b-3b1b-422c-957f-cfeae36b472d}": {
            "name": "vm-with-snapshot-lin",
            "date": "2026-07-10 11:41:38",
            "state": "poweron",
            "current": true,
            "parent": ""
        }
        }"#;
        assert_eq!(
            parse_snapshot_list(json).unwrap(),
            vec![(
                "{351b744b-3b1b-422c-957f-cfeae36b472d}".to_string(),
                "vm-with-snapshot-lin".to_string()
            )]
        );
    }

    #[test]
    fn parses_vm_details() {
        // Trimmed from real `prlctl list -i --json` output (Parallels 26.4).
        let json = r#"[{
            "Name": "macOS",
            "Home": "/Users/hakesson/Parallels/macOS.macvm/",
            "Hardware": {
                "cpu": {"cpus": 10},
                "memory": {"size": "20480Mb", "auto": "off", "hotplug": false}
            }
        }]"#;
        let d = parse_details(json).unwrap();
        assert_eq!(d.home, "/Users/hakesson/Parallels/macOS.macvm/");
        assert_eq!(d.memory_mb, 20480);
    }

    #[test]
    fn link_local_ipv6_is_not_an_ip_yet() {
        // Seen live: a resuming Windows guest reports its link-local IPv6
        // before DHCP completes; ssh to it fails with "No route to host".
        let vm = PrlVm {
            uuid: "{x}".into(),
            status: "running".into(),
            ip_configured: "fe80::bcca:2118:95a7:5e25".into(),
            name: "Windows 11".into(),
        };
        assert_eq!(vm.ip(), None);
    }
}
