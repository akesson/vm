use crate::config::{Config, GuestOs};
use crate::proto::{PROTO_VERSION, VersionInfo};
use crate::ssh::SshTarget;
use crate::{commands, mapping, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;

/// Build and install the vm agent inside a guest ("build-in-guest").
///
/// The agent source is this binary's own repo (baked in at compile time —
/// dev-mode deployment; release-asset download can replace this later). The
/// source travels with the same git-object sync used for user repos, but the
/// guest side is driven with plain ssh git commands because the agent does
/// not exist yet. Incremental: the guest keeps its target/ between deploys.
pub fn deploy(alias: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let vm = cfg.get(alias)?;
    let src = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !src.join("Cargo.toml").exists() {
        bail!(
            "vm source not found at {} (deploy currently builds from the repo \
             this binary was compiled from)",
            src.display()
        );
    }

    prl::ensure_running(&vm.parallels_name)?;
    prl::wait_for_ip(&vm.parallels_name, Duration::from_secs(90))?;
    let target = commands::ssh_target(vm)?;
    eprintln!(
        "vm ▸ {alias} ▸ deploying agent (build-in-guest from {})…",
        src.display()
    );

    // Every guest presents a POSIX shell over ssh (Windows: Git Bash as the
    // sshd DefaultShell), so deployment is one code path; only the binary
    // file name differs.
    let bin = match vm.os {
        GuestOs::Windows => "vm.exe",
        GuestOs::Linux | GuestOs::Macos => "vm",
    };
    build_in_guest(alias, &target, src, bin)?;

    // Handshake: the freshly installed agent must speak our protocol.
    let reply = commands::agent_call(vm, &target, &["_version"])?;
    let info: VersionInfo = serde_json::from_str(&reply).context("parsing agent _version")?;
    if info.proto != PROTO_VERSION {
        bail!(
            "deployed agent speaks proto v{} but host needs v{PROTO_VERSION}",
            info.proto
        );
    }
    eprintln!(
        "vm ▸ {alias} ▸ agent v{} (proto v{}) installed",
        info.binary, info.proto
    );
    Ok(0)
}

/// Run a controlled remote shell line, streaming output (used for builds).
fn remote(target: &SshTarget, line: &str) -> Result<()> {
    let status = ssh::ssh_command(target)
        .arg(line)
        .status()
        .context("failed to spawn ssh")?;
    if !status.success() {
        bail!("remote step failed (exit {:?}): {line}", status.code());
    }
    Ok(())
}

fn build_in_guest(alias: &str, target: &SshTarget, src: &Path, bin: &str) -> Result<()> {
    remote(
        target,
        "mkdir -p ~/.vm/bin && { test -d ~/.vm/src/.git || git init -q ~/.vm/src; } \
         && git -C ~/.vm/src config core.autocrlf false",
    )?;
    let url = mapping::ssh_remote_url(&target.user, &target.host, "~/.vm/src");
    let snap = sync::host::sync_to(
        src,
        &format!("deploy-{alias}"),
        &url,
        Some(&ssh::git_ssh_command()),
    )?;
    remote(
        target,
        &format!(
            "git -C ~/.vm/src reset --hard -q {} && git -C ~/.vm/src clean -fdq",
            snap.commit
        ),
    )?;
    eprintln!("vm ▸ {alias} ▸ building agent in guest (first build takes a while)…");
    // Install via rename: replacing a currently-executing agent binary fails
    // with `cp` (busy) but a rename always succeeds.
    remote(
        target,
        &format!(
            "cd ~/.vm/src && PATH=\"$HOME/.cargo/bin:$PATH\" cargo build --release \
             && cp target/release/{bin} ~/.vm/bin/{bin}.new \
             && mv -f ~/.vm/bin/{bin}.new ~/.vm/bin/{bin}"
        ),
    )
}
