use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::process::Command;
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
    pub fn ip(&self) -> Option<&str> {
        (self.ip_configured != "-").then_some(self.ip_configured.as_str())
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

pub fn stop(name: &str, kill: bool) -> Result<()> {
    let args: &[&str] = if kill {
        &["stop", name, "--kill"]
    } else {
        &["stop", name]
    };
    prlctl(args).map(drop)
}

pub fn suspend(name: &str) -> Result<()> {
    prlctl(&["suspend", name]).map(drop)
}

/// Screenshot the VM display to a PNG file.
pub fn capture(name: &str, file: &str) -> Result<()> {
    prlctl(&["capture", name, "--file", file]).map(drop)
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
}
