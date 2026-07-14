use crate::config::{Config, GuestOs};
use crate::proto::{PROTO_VERSION, VersionInfo};
use crate::ssh::SshTarget;
use crate::{commands, crumb, mapping, prldnd, ssh, sync};
use anyhow::{Context, Result, bail};
use std::path::Path;

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

    let _use = crate::lock::shared(alias)?;
    let target = commands::bring_up(alias, vm)?;
    crumb!(
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
    crumb!(
        "vm ▸ {alias} ▸ agent v{} (proto v{}) installed",
        info.binary,
        info.proto
    );

    // Deploy is where a guest gets provisioned, and this is the other thing a
    // usable linux guest needs: without it every shutdown takes ~95s and lands
    // 20s short of Parallels' 120s force-kill (see `crate::prldnd`). A failure
    // here is fatal rather than a warning — a guest that quietly kept the hang
    // would look deployed and behave like a hazard.
    if vm.os == GuestOs::Linux {
        prldnd::install(alias, vm)?;
    }
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
    // Serialize concurrent deploys of this alias against each other: they share
    // the host source snapshot index (`vm-sync-index-deploy-<alias>`), the
    // guest `~/.vm/src` checkout, and the `~/.vm/bin` install target. Held for
    // the whole build so two deploys can't clobber each other's binary.
    let _sync_guard = sync::host::lock_sync(src, &format!("deploy-{alias}"))?;
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
        // vm's own source: nothing gitignored belongs in the build.
        &[],
    )?;
    remote(
        target,
        &format!(
            "git -C ~/.vm/src reset --hard -q {} && git -C ~/.vm/src clean -fdq",
            snap.commit
        ),
    )?;
    crumb!("vm ▸ {alias} ▸ building agent in guest (first build takes a while)…");
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
