use anyhow::{Context, Result};
use std::process::{Command, Output, Stdio};

/// Where to ssh: guest user + reachable address.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub user: String,
    pub host: String,
}

impl SshTarget {
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Shared client options. ControlMaster multiplexing keeps per-command
/// latency ~10ms after the first connection; BatchMode makes auth failures
/// fail fast instead of hanging on a password prompt. VM host keys live in a
/// dedicated known-hosts file so VM rebuilds never poison the user's real
/// known_hosts.
const SSH_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=5",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "UserKnownHostsFile=~/.ssh/vm-known-hosts",
    "-o",
    "ControlMaster=auto",
    "-o",
    "ControlPath=~/.ssh/vm-cm-%C",
    "-o",
    "ControlPersist=600",
    "-o",
    "LogLevel=ERROR",
];

/// Base ssh invocation for a target.
pub fn ssh_command(target: &SshTarget) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTIONS).arg(target.destination());
    cmd
}

/// The same options as a GIT_SSH_COMMAND string, so `git push`/`fetch`
/// reuse the multiplexed connection and the dedicated known-hosts file.
pub fn git_ssh_command() -> String {
    let mut s = String::from("ssh");
    for opt in SSH_OPTIONS {
        s.push(' ');
        s.push_str(opt);
    }
    s
}

/// Run a remote command, capturing output (for plumbing calls).
pub fn run_capture(target: &SshTarget, remote: &[&str]) -> Result<Output> {
    ssh_command(target)
        .args(remote)
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn ssh")
}

/// Quick reachability probe (`ssh … true`).
pub fn reachable(target: &SshTarget) -> bool {
    run_capture(target, &["true"])
        .map(|out| out.status.success())
        .unwrap_or(false)
}
