//! `vm doctor [alias]` — diagnose host and guest setup, one check per line.
//!
//! Read-only: never starts a VM or installs anything; a stopped VM simply
//! skips its live checks with a hint.

use crate::config::Config;
use crate::proto::{PROTO_VERSION, VersionInfo};
use crate::{commands, prl, ssh};
use anyhow::Result;

struct Report {
    failures: u32,
}

impl Report {
    fn ok(&mut self, what: &str, detail: &str) {
        eprintln!("  ✓ {what}: {detail}");
    }
    fn fail(&mut self, what: &str, detail: &str) {
        self.failures += 1;
        eprintln!("  ✗ {what}: {detail}");
    }
    fn skip(&mut self, what: &str, detail: &str) {
        eprintln!("  - {what}: {detail}");
    }
}

pub fn doctor(alias: Option<&str>) -> Result<i32> {
    let mut r = Report { failures: 0 };

    eprintln!("host");
    let vms = match prl::list_all() {
        Ok(vms) => {
            r.ok("prlctl", &format!("{} VMs registered", vms.len()));
            vms
        }
        Err(err) => {
            r.fail("prlctl", &format!("{err:#}"));
            vec![]
        }
    };
    let cfg = match Config::load() {
        Ok(cfg) => {
            let aliases: Vec<&str> = cfg.vm.keys().map(String::as_str).collect();
            r.ok(
                "config",
                &format!("{} ({})", Config::path().display(), aliases.join(", ")),
            );
            cfg
        }
        Err(err) => {
            r.fail("config", &format!("{err:#}"));
            eprintln!("vm ▸ doctor ▸ {} problem(s)", r.failures);
            return Ok(1);
        }
    };
    let key = crate::sync::expand_home("~/.ssh/id_ed25519")?;
    if key.exists() {
        r.ok("ssh key", &key.display().to_string());
    } else {
        r.fail(
            "ssh key",
            "~/.ssh/id_ed25519 missing — `ssh-keygen -t ed25519`",
        );
    }
    if crate::reap::launchd_loaded() {
        r.ok("reap timer", crate::reap::LAUNCHD_LABEL);
    } else {
        r.skip(
            "reap timer",
            "not installed — `vm reap --install` auto-suspends idle VMs",
        );
    }

    for (name, vm) in &cfg.vm {
        if alias.is_some_and(|a| a != name) {
            continue;
        }
        eprintln!("{name} ({})", vm.parallels_name);
        let Some(prl_vm) = vms.iter().find(|p| p.name == vm.parallels_name) else {
            r.fail("vm", "not registered in Parallels (`prlctl list -a`)");
            continue;
        };
        match prl::snapshot_list(&vm.parallels_name) {
            Ok(snaps) => {
                let stale: Vec<&str> = snaps
                    .iter()
                    .filter(|(_, n)| n.starts_with("vm-with-snapshot-"))
                    .map(|(_, n)| n.as_str())
                    .collect();
                if !stale.is_empty() {
                    r.fail(
                        "snapshots",
                        &format!(
                            "stale from killed with-snapshot runs, wasting disk: {} — \
                             next `vm with-snapshot {name}` sweeps them, or delete via \
                             `prlctl snapshot-list`/`snapshot-delete`",
                            stale.join(", ")
                        ),
                    );
                }
            }
            Err(err) => r.fail("snapshots", &format!("{err:#}")),
        }
        if prl_vm.status != "running" {
            r.skip(
                "status",
                &format!("{} — `vm start {name}` for live checks", prl_vm.status),
            );
            continue;
        }
        r.ok("status", "running");
        let target = match commands::ssh_target(vm) {
            Ok(t) => t,
            Err(err) => {
                r.fail("ip", &format!("{err:#}"));
                continue;
            }
        };
        if !ssh::reachable(&target) {
            r.fail(
                "ssh",
                &format!(
                    "{} not reachable (sshd? firewall? key?)",
                    target.destination()
                ),
            );
            continue;
        }
        r.ok("ssh", &target.destination());

        match commands::agent_call(vm, &target, &["_version"]) {
            Ok(json) => match serde_json::from_str::<VersionInfo>(&json) {
                Ok(v) if v.proto == PROTO_VERSION => {
                    r.ok("agent", &format!("v{} (proto v{})", v.binary, v.proto));
                }
                Ok(v) => r.fail(
                    "agent",
                    &format!(
                        "speaks proto v{}, host needs v{PROTO_VERSION} — `vm deploy {name}`",
                        v.proto
                    ),
                ),
                Err(err) => r.fail("agent", &format!("bad _version reply: {err:#}")),
            },
            Err(err) => r.fail("agent", &format!("{err:#} — `vm deploy {name}`")),
        }

        match ssh::run_capture(&target, &["git", "--version"]) {
            Ok(out) if out.status.success() => {
                r.ok("git", String::from_utf8_lossy(&out.stdout).trim());
            }
            _ => r.fail("git", "not on the guest's ssh PATH (sync needs it)"),
        }

        let root = ssh::shell_quote(&vm.work_root);
        let probe = format!("mkdir -p {root} && test -w {root}");
        match ssh::run_capture(&target, &["sh", "-c", &ssh::shell_quote(&probe)]) {
            Ok(out) if out.status.success() => r.ok("work_root", &vm.work_root),
            _ => r.fail("work_root", &format!("{} not writable", vm.work_root)),
        }
    }

    if r.failures == 0 {
        eprintln!("vm ▸ doctor ▸ all checks passed");
        Ok(0)
    } else {
        eprintln!("vm ▸ doctor ▸ {} problem(s)", r.failures);
        Ok(1)
    }
}
