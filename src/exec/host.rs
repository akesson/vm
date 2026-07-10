use crate::config::{Config, GuestOs, VmConfig};
use crate::proto::{ExecRequest, PROTO_VERSION};
use crate::{commands, mapping, prl, ssh, sync};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Lib-side mirror of the CLI exec flags.
pub struct ExecOptions {
    pub no_sync: bool,
    pub writeback: bool,
    pub shell: bool,
    pub cmd: Vec<String>,
}

/// `vm exec <alias|os> -- cmd…`: sync, run in the guest checkout, propagate
/// exit. The target is an alias or an OS name; either way it is always a VM
/// — even when the OS named is the host's own — so a `vm` invocation never
/// silently runs on the host (scripts that want native execution just run
/// the command directly).
pub fn exec(target: &str, opts: &ExecOptions) -> Result<i32> {
    let cfg = Config::load()?;
    let (alias, vm) = cfg.resolve(target)?;
    // Registers this run as a use of the VM: stop/with-snapshot/reap keep
    // their hands off until it finishes. Blocks briefly if one of those is
    // mid-flight right now.
    let _use = crate::lock::shared(alias)?;
    exec_in(alias, vm, opts)
}

/// `exec` without registering a use — only for a caller that already holds
/// the VM's exclusive lock (with-snapshot), where `exec` would deadlock.
pub fn exec_unlocked(target: &str, opts: &ExecOptions) -> Result<i32> {
    let cfg = Config::load()?;
    let (alias, vm) = cfg.resolve(target)?;
    exec_in(alias, vm, opts)
}

fn exec_in(alias: &str, vm: &VmConfig, opts: &ExecOptions) -> Result<i32> {
    prl::ensure_running(&vm.parallels_name)?;
    prl::wait_for_ip(&vm.parallels_name, Duration::from_secs(90))?;
    let target = commands::ssh_target(vm)?;
    let repo = mapping::RepoLocation::discover()?;

    let base = if opts.no_sync {
        None
    } else {
        Some(commands::sync_repo(alias, vm, &target)?)
    };

    let cwd = mapping::guest_cwd(&vm.work_root, &repo.name, &repo.rel)?;
    let req = ExecRequest {
        version: PROTO_VERSION,
        argv: opts.cmd.clone(),
        env: BTreeMap::new(),
        cwd: cwd.clone(),
        shell: opts.shell,
    };

    eprintln!(
        "vm ▸ {alias} ({}) ▸ {cwd} ▸ $ {}",
        vm.parallels_name,
        opts.cmd.join(" ")
    );
    let started = Instant::now();

    let mut child = agent_exec_command(vm, &target)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn the exec transport")?;
    let mut request_line = serde_json::to_string(&req)?;
    request_line.push('\n');
    // Take stdin OUT of the Child: Child::wait() closes child.stdin before
    // blocking, and this pipe is the liveness channel — it must stay open for
    // the whole run. If this process dies (Ctrl-C, kill), the pipe closes,
    // the agent's watcher sees EOF, and the guest kills the child tree.
    let mut agent_stdin = child.stdin.take().expect("piped stdin");
    agent_stdin.write_all(request_line.as_bytes())?;
    let status = child.wait()?;
    drop(agent_stdin);
    let code = status.code().unwrap_or(255);

    if code == 127 {
        eprintln!("vm ▸ {alias} ▸ exit 127 — is the agent installed? try `vm deploy {alias}`");
    }

    if opts.writeback
        && let Some(base) = &base
        && code != 255
    {
        writeback(alias, vm, &target, &repo, base)?;
    }

    eprintln!(
        "vm ▸ {alias} ▸ exit {code} ▸ {:.1}s",
        started.elapsed().as_secs_f32()
    );
    Ok(code)
}

/// The host process that carries an ExecRequest to the guest agent. Unix
/// guests go over ssh. Windows goes through `prlctl exec --current-user`
/// instead: sshd puts children in session 0 on a non-interactive window
/// station, where UIA (and any GUI automation) sees an empty desktop, while
/// Parallels Tools injects into the console session. Same agent, same
/// protocol — stdout/stderr stream and stdin stays the liveness channel
/// either way.
fn agent_exec_command(vm: &VmConfig, target: &ssh::SshTarget) -> std::process::Command {
    match vm.os {
        GuestOs::Windows => {
            let mut cmd = prl::exec_console(&vm.parallels_name);
            // Through cmd.exe so %USERPROFILE% in the agent path expands.
            cmd.args([
                "cmd",
                "/c",
                &format!("{} _exec", commands::agent_console_path(vm)),
            ]);
            cmd
        }
        GuestOs::Linux | GuestOs::Macos => {
            let mut cmd = ssh::ssh_command(target);
            cmd.arg(commands::agent_path(vm)).arg("_exec");
            cmd
        }
    }
}

fn writeback(
    alias: &str,
    vm: &VmConfig,
    target: &ssh::SshTarget,
    repo: &mapping::RepoLocation,
    base: &sync::Snapshot,
) -> Result<()> {
    // Same critical section as the forward sync: the guest's writeback
    // snapshot index and refs/sync/writeback, plus the patch applied back onto
    // the host tree. Not held across the guest command run in between (parallel
    // execs on one VM must stay parallel) — only around sync and writeback.
    let _sync_guard = sync::host::lock_sync(&repo.root, alias)?;
    let guest_repo = mapping::guest_repo_path(&vm.work_root, &repo.name);
    let json = commands::agent_call(vm, target, &["_tree", "--repo", &guest_repo])?;
    let wb: sync::Snapshot = serde_json::from_str(&json).context("parsing _tree reply")?;
    let url = mapping::ssh_remote_url(&target.user, &target.host, &guest_repo);
    let applied =
        sync::host::apply_writeback(&repo.root, &url, base, &wb, Some(&ssh::git_ssh_command()))?;
    if applied {
        eprintln!("vm ▸ {alias} ▸ writeback applied to host tree");
    }
    Ok(())
}
