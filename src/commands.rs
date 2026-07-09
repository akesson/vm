use crate::config::{Config, GuestOs, VmConfig};
use crate::ssh::SshTarget;
use crate::{mapping, prl, ssh, sync};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

impl std::fmt::Display for GuestOs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // f.pad (not write_str) so `{:<8}` column widths apply.
        f.pad(match self {
            GuestOs::Windows => "windows",
            GuestOs::Linux => "linux",
            GuestOs::Macos => "macos",
        })
    }
}

/// Resolve the address to ssh to: config override, or the IP Parallels reports.
#[allow(dead_code)] // used from phase 4 (exec)
pub fn ssh_target(vm: &VmConfig) -> Result<SshTarget> {
    let host = match &vm.host {
        Some(host) => host.clone(),
        None => {
            let prl = prl::find(&vm.parallels_name)?;
            prl.ip()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "VM '{}' has no IP (status: {}); `vm start` it first",
                        vm.parallels_name,
                        prl.status
                    )
                })?
                .to_string()
        }
    };
    Ok(SshTarget {
        user: vm.user.clone(),
        host,
    })
}

pub fn ls() -> Result<i32> {
    let cfg = Config::load()?;
    let vms = prl::list_all()?;
    let repo = mapping::RepoLocation::discover().ok();

    println!(
        "{:<7} {:<8} {:<14} {:<10} {:<15} CHECKOUT (this repo)",
        "ALIAS", "OS", "VM", "STATUS", "IP"
    );
    for (alias, vm) in &cfg.vm {
        let prl_vm = vms.iter().find(|p| p.name == vm.parallels_name);
        let status = prl_vm.map_or("missing!", |p| p.status.as_str());
        let ip = prl_vm.and_then(|p| p.ip()).unwrap_or("-");
        let checkout = match &repo {
            Some(repo) => mapping::guest_repo_path(vm.os, &vm.work_root, &repo.name),
            None => "- (not in a git repo)".to_string(),
        };
        println!(
            "{:<7} {:<8} {:<14} {:<10} {:<15} {}",
            alias, vm.os, vm.parallels_name, status, ip, checkout
        );
    }
    Ok(0)
}

pub fn start(alias: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    prl::ensure_running(&vm.parallels_name)?;
    let ip = prl::wait_for_ip(&vm.parallels_name, Duration::from_secs(90))?;
    let target = SshTarget {
        user: vm.user.clone(),
        host: vm.host.clone().unwrap_or(ip),
    };

    // Wait for sshd so a following `vm exec` never races the boot.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if ssh::reachable(&target) {
            eprintln!(
                "vm ▸ {alias} ({}) ▸ running ▸ ssh {}",
                vm.parallels_name,
                target.destination()
            );
            return Ok(0);
        }
        if Instant::now() >= deadline {
            eprintln!(
                "vm ▸ {alias} ▸ running, but ssh to {} not reachable after 60s \
                 (guest sshd not set up? see `vm doctor {alias}`)",
                target.destination()
            );
            return Ok(0);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// The vm agent binary inside the guest. `~`/`%USERPROFILE%` are expanded by
/// the remote shell (sh on unix, cmd.exe on Windows).
pub fn agent_path(vm: &VmConfig) -> String {
    match &vm.agent_path {
        Some(path) => path.clone(),
        None => match vm.os {
            GuestOs::Windows => r"%USERPROFILE%\.vm\bin\vm.exe".to_string(),
            GuestOs::Linux | GuestOs::Macos => "~/.vm/bin/vm".to_string(),
        },
    }
}

/// Invoke a hidden agent verb in the guest, capturing stdout. Fails with the
/// agent's stderr, or a deploy hint when the binary is missing.
pub fn agent_call(vm: &VmConfig, target: &SshTarget, args: &[&str]) -> Result<String> {
    let agent = agent_path(vm);
    let mut remote: Vec<&str> = vec![&agent];
    remote.extend_from_slice(args);
    let out = ssh::run_capture(target, &remote)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No such file")
            || stderr.contains("not recognized")
            || stderr.contains("not found")
        {
            bail!("vm agent not installed in guest (run `vm deploy` first)");
        }
        bail!(
            "guest agent {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

/// Full sync of the current repo to a guest. Returns the verified snapshot.
pub fn sync_repo(alias: &str, vm: &VmConfig, target: &SshTarget) -> Result<sync::Snapshot> {
    let repo = mapping::RepoLocation::discover()?;
    let guest_repo = mapping::guest_repo_path(vm.os, &vm.work_root, &repo.name);
    let started = Instant::now();

    // 1. Make sure the guest repository exists (idempotent, cheap).
    agent_call(vm, target, &["_sync-init", "--repo", &guest_repo])?;

    // 2. Snapshot + push objects.
    let url = mapping::ssh_remote_url(&target.user, &target.host, &guest_repo);
    let snap = sync::host::sync_to(&repo.root, alias, &url, Some(&ssh::git_ssh_command()))?;

    // 3. Apply in the guest and verify the tree hash round-trips.
    let guest_tree = agent_call(
        vm,
        target,
        &["_sync-apply", "--repo", &guest_repo, "--sha", &snap.commit],
    )?;
    if guest_tree != snap.tree {
        bail!(
            "sync verification failed: host tree {} but guest reports {guest_tree}",
            snap.tree
        );
    }
    eprintln!(
        "vm ▸ {alias} ▸ synced {} ▸ tree {} ▸ {:.1}s",
        repo.name,
        &snap.tree[..10],
        started.elapsed().as_secs_f32()
    );
    Ok(snap)
}

/// `vm sync <alias>`: start the VM if needed, then sync.
pub fn sync_cmd(alias: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    prl::ensure_running(&vm.parallels_name)?;
    prl::wait_for_ip(&vm.parallels_name, Duration::from_secs(90))?;
    let target = ssh_target(vm)?;
    sync_repo(alias, vm, &target)?;
    Ok(0)
}

pub fn stop(alias: &str, kill: bool) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    prl::stop(&vm.parallels_name, kill)?;
    eprintln!("vm ▸ {alias} ▸ stopped");
    Ok(0)
}

pub fn suspend(alias: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    if vm.os == GuestOs::Macos {
        bail!("Parallels cannot suspend macOS guests on Apple Silicon; use `vm stop {alias}`");
    }
    prl::suspend(&vm.parallels_name)?;
    eprintln!("vm ▸ {alias} ▸ suspended");
    Ok(0)
}
