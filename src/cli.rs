use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Run commands in Parallels VMs against a synced copy of the current repo.
///
/// The host working tree is the single source of truth. Before every exec,
/// the repo is snapshotted (dirty state included, staging area untouched)
/// and pushed to a per-guest native checkout, so guests always see exactly
/// what is on disk on the host. Builds run on guest-local disk: no shared
/// folders, no cross-platform artifact conflicts.
#[derive(Parser)]
#[command(name = "vm", version, about, max_term_width = 100)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List configured VMs: status, IP, and the guest checkout path of the current repo
    Ls,
    /// Start (or resume) a VM and wait until it is reachable
    Start { alias: String },
    /// Stop a VM gracefully
    Stop {
        alias: String,
        /// Force power-off instead of a graceful shutdown
        #[arg(long)]
        kill: bool,
    },
    /// Suspend a VM
    Suspend { alias: String },
    /// Sync the current repo to a guest, then run a command in the guest checkout
    Exec(ExecArgs),
    /// Sync the current repo to a guest without running anything
    Sync { alias: String },
    /// Build and install the vm agent inside a guest
    Deploy { alias: String },
    /// Save a screenshot of a VM's display
    Shot {
        alias: String,
        /// Output file (default: <alias>-<timestamp>.png in the current dir)
        file: Option<PathBuf>,
    },
    /// Snapshot a VM, run a command, then roll back
    WithSnapshot(ExecArgs),
    /// Diagnose host and guest setup
    Doctor {
        /// Check a single VM (default: all configured VMs)
        alias: Option<String>,
    },
    /// Remove the guest checkout of the current repo
    Clean { alias: String },

    // Hidden guest-side verbs, invoked peer-to-peer by a host `vm` over ssh.
    /// Run a command described by an ExecRequest (JSON on stdin)
    #[command(name = "_exec", hide = true)]
    GuestExec,
    /// Create and configure the local checkout repository if missing
    #[command(name = "_sync-init", hide = true)]
    GuestSyncInit {
        /// Path of the guest checkout ('~/' prefix allowed)
        #[arg(long)]
        repo: String,
    },
    /// Apply a synced commit to the local checkout and print its tree hash
    #[command(name = "_sync-apply", hide = true)]
    GuestSyncApply {
        /// Absolute path of the guest checkout ('~/' prefix allowed)
        #[arg(long)]
        repo: String,
        /// Commit sha previously pushed to refs/sync/head
        #[arg(long)]
        sha: String,
    },
    /// Snapshot the local checkout (for writeback) and print commit + tree hash
    #[command(name = "_tree", hide = true)]
    GuestTree {
        #[arg(long)]
        repo: String,
    },
    /// Print the agent's protocol version and binary version
    #[command(name = "_version", hide = true)]
    GuestVersion,
}

#[derive(Args)]
pub struct ExecArgs {
    /// VM alias from ~/.config/vm/config.toml, or an OS name
    /// (windows | linux | macos) to pick the VM configured for that OS
    pub target: String,
    #[command(flatten)]
    pub opts: ExecOpts,
}

#[derive(Args)]
pub struct ExecOpts {
    /// Skip the pre-exec sync (run against the guest checkout as-is)
    #[arg(long)]
    pub no_sync: bool,
    /// After the command, apply source changes made in the guest back to the host tree
    #[arg(long)]
    pub writeback: bool,
    /// Run through the guest shell (enables pipes/redirection; argv is joined)
    #[arg(long)]
    pub shell: bool,
    /// Command and arguments to run in the guest checkout
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn exec_keeps_hyphen_args_after_command() {
        let cli = parse(&["vm", "exec", "win", "--", "cargo", "build", "--release"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.target, "win");
        assert_eq!(exec.opts.cmd, ["cargo", "build", "--release"]);
        assert!(!exec.opts.shell);
    }

    #[test]
    fn exec_flags_before_command() {
        let cli = parse(&["vm", "exec", "win", "--no-sync", "--", "echo", "hi"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert!(exec.opts.no_sync);
        assert_eq!(exec.opts.cmd, ["echo", "hi"]);
    }

    #[test]
    fn exec_target_can_be_an_os_name() {
        let cli = parse(&["vm", "exec", "windows", "--", "cargo", "nextest", "run"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.target, "windows");
        assert_eq!(exec.opts.cmd, ["cargo", "nextest", "run"]);
    }

    #[test]
    fn exec_requires_a_command() {
        assert!(Cli::try_parse_from(["vm", "exec", "win"]).is_err());
    }

    #[test]
    fn exec_requires_a_target() {
        assert!(Cli::try_parse_from(["vm", "exec"]).is_err());
    }

    #[test]
    fn guest_verbs_parse() {
        let cli = parse(&[
            "vm",
            "_sync-apply",
            "--repo",
            "~/work/syncfs",
            "--sha",
            "abc123",
        ]);
        let Command::GuestSyncApply { repo, sha } = cli.command else {
            panic!("expected _sync-apply");
        };
        assert_eq!(repo, "~/work/syncfs");
        assert_eq!(sha, "abc123");
    }
}
