use crate::config::{Config, GuestOs, VmConfig};
use crate::proto::{ExecRequest, PROTO_VERSION};
use crate::{commands, mapping, prl, ssh, sync};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Lib-side mirror of the CLI exec flags.
pub struct ExecOptions {
    pub no_sync: bool,
    pub writeback: bool,
    pub shell: bool,
    /// `NAME=value` / bare `NAME` specs from `-e`; resolved against the host
    /// environment when the request is built.
    pub env: Vec<String>,
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

    // Runs the repo's `on_first_sync` hook the first time this checkout exists
    // (and after `vm clean`), before the user's command. No-op otherwise. Also
    // covers `--no-sync` against a checkout that never ran it.
    commands::first_sync_hook(alias, vm, &target, &repo)?;

    let cwd = mapping::guest_cwd(&vm.work_root, &repo.name, &repo.rel)?;
    let env = resolve_env(&opts.env, |name| std::env::var(name).ok())?;
    let req = ExecRequest {
        version: PROTO_VERSION,
        argv: opts.cmd.clone(),
        env,
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
    let status = child.wait().context("waiting on the exec transport")?;
    drop(agent_stdin);
    let code = match status.code() {
        Some(code) => code,
        // No exit code means the transport itself was killed by a signal while
        // this process survived (the connection dropped, or ssh/prlctl was
        // signalled) — a vm infra failure, not a result from the guest command.
        None => bail!(
            "the exec transport to {alias} was killed before the guest reported an \
             exit status — the connection dropped, or ssh/prlctl was signalled"
        ),
    };

    // 127 now doubles as the guest reporting command-not-found (see
    // exec/guest.rs), so keep the hint suggestive rather than assertive.
    if code == 127 {
        eprintln!(
            "vm ▸ {alias} ▸ exit 127 — command not found in the guest \
             (or the agent is missing — try `vm deploy {alias}`)"
        );
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

/// Resolve `-e` specs into an explicit NAME→value map for the guest process.
/// `NAME=value` sets the variable directly (the value may be empty or itself
/// contain `=`). Bare `NAME` forwards the host's current value and errors if
/// it is unset — an explicit request gets explicit feedback. On a duplicate
/// name the last spec wins.
fn resolve_env(
    specs: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for spec in specs {
        match spec.split_once('=') {
            Some(("", _)) => bail!("-e {spec}: empty variable name"),
            Some((name, value)) => {
                env.insert(name.to_string(), value.to_string());
            }
            None => {
                let value = lookup(spec).ok_or_else(|| {
                    anyhow::anyhow!(
                        "-e {spec}: not set on host (use -e {spec}=value to set it explicitly)"
                    )
                })?;
                env.insert(spec.clone(), value);
            }
        }
    }
    Ok(env)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A host environment stub, so the tests never touch the real process env
    /// (mutating it is `unsafe` on edition 2024 and races other tests).
    fn host<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_assignment_sets_the_value() {
        let env = resolve_env(&s(&["FOO=bar"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn value_may_contain_equals_signs() {
        let env = resolve_env(&s(&["FOO=a=b"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("a=b"));
    }

    #[test]
    fn empty_value_is_allowed() {
        let env = resolve_env(&s(&["FOO="]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some(""));
    }

    #[test]
    fn bare_name_forwards_the_host_value() {
        let env = resolve_env(&s(&["FOO"]), host(&[("FOO", "from-host")])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("from-host"));
    }

    #[test]
    fn bare_name_unset_on_host_is_an_error() {
        let err = resolve_env(&s(&["FOO"]), host(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("FOO"), "{err}");
        assert!(err.contains("not set on host"), "{err}");
        assert!(err.contains("FOO=value"), "{err}");
    }

    #[test]
    fn empty_name_is_an_error() {
        let err = resolve_env(&s(&["=value"]), host(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty variable name"), "{err}");
    }

    #[test]
    fn duplicate_name_takes_the_last_spec() {
        let env = resolve_env(&s(&["FOO=1", "FOO=2"]), host(&[])).unwrap();
        assert_eq!(env.get("FOO").map(String::as_str), Some("2"));
    }
}
