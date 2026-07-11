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
    Start {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
    },
    /// Stop a VM gracefully (refuses while other vm processes are using it)
    Stop {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
        /// Force power-off instead of a graceful shutdown
        #[arg(long)]
        kill: bool,
        /// Stop even while other vm processes are using the VM
        #[arg(long)]
        force: bool,
    },
    /// Sync the current repo to a guest, then run a command in the guest checkout
    ///
    /// Exit status is the guest command's own, except vm's reserved codes:
    /// 125 = vm infrastructure error (sync/agent/ssh/VM lifecycle), 2 =
    /// usage/config error. See the README's "Exit codes" section.
    Exec(ExecArgs),
    /// Sync the current repo to a guest without running anything
    Sync {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
    },
    /// Build and install the vm agent inside a guest
    Deploy {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
    },
    /// Save a screenshot of a VM's display
    Shot {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
        /// Output file (default: <alias>-<timestamp>.png in the current dir)
        file: Option<PathBuf>,
    },
    /// Snapshot a VM, run a command, then roll back
    WithSnapshot(ExecArgs),
    /// Run Claude Code headless in the guest checkout of the current repo
    ///
    /// Syncs the repo, runs `claude -p` with permission prompts bypassed —
    /// the VM is the permission boundary — and applies source edits back to
    /// the host tree (writeback) when it finishes. Requires the claude CLI
    /// installed and authenticated in the guest (`vm doctor` verifies).
    Claude(ClaudeArgs),
    /// Suspend VMs that no vm process is using and that have been idle a while
    /// (any `vm exec` resumes them in about a second)
    Reap {
        /// Only consider this VM (default: all configured VMs)
        alias: Option<String>,
        /// Idle threshold in minutes, measured from the end of the last use
        #[arg(long, default_value_t = 30)]
        idle_minutes: u64,
        /// Install a launchd job running `vm reap` every 5 minutes
        #[arg(long, conflicts_with_all = ["uninstall", "alias"])]
        install: bool,
        /// Remove the launchd job
        #[arg(long, conflicts_with = "alias")]
        uninstall: bool,
    },
    /// Diagnose host and guest setup
    Doctor {
        /// Check a single VM (default: all configured VMs)
        alias: Option<String>,
    },
    /// Remove the guest checkout of the current repo
    Clean {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
    },

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
    /// Run the repo's first-sync hook in the checkout, once per checkout creation
    #[command(name = "_first-sync", hide = true)]
    GuestFirstSync {
        /// Path of the guest checkout ('~/' prefix allowed)
        #[arg(long)]
        repo: String,
        /// The `on_first_sync` command from the repo's .vm.toml
        #[arg(long)]
        cmd: String,
    },
    /// Print the agent's protocol version and binary version
    #[command(name = "_version", hide = true)]
    GuestVersion,
    /// Print milliseconds since the last local input event (reap's console-use probe)
    #[command(name = "_idle", hide = true)]
    GuestIdle,
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
pub struct ClaudeArgs {
    /// VM alias from ~/.config/vm/config.toml, or an OS name
    /// (windows | linux | macos) to pick the VM configured for that OS
    pub target: String,
    /// Prompt for the headless run
    pub prompt: String,
    /// Snapshot the VM first and roll it back afterwards — the guest keeps
    /// nothing; only the writeback diff survives the run
    #[arg(long)]
    pub with_snapshot: bool,
    /// Leave Claude's source edits in the guest instead of applying them
    /// back to the host tree
    #[arg(long)]
    pub no_writeback: bool,
    /// Extra arguments passed to claude verbatim (e.g. --model sonnet)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub claude_args: Vec<String>,
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
    /// If the host OS already matches the target's os, run the command
    /// natively (no VM, no sync, no guest) instead of in the guest — the same
    /// task then works on a dev host driving a guest and on a CI runner that
    /// is already the target OS. Omit it to force the VM even on a matching
    /// host (e.g. a macOS host driving the mac guest for UI tests).
    #[arg(long)]
    pub or_native: bool,
    /// Set an env var for the guest command: `-e NAME=value`, or `-e NAME`
    /// to forward the host's current value. Repeatable.
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
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
    fn exec_collects_repeated_env_flags_before_the_command() {
        let cli = parse(&[
            "vm", "exec", "win", "-e", "FOO=bar", "--env", "BAZ", "--", "cargo", "test",
        ]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.env, ["FOO=bar", "BAZ"]);
        assert_eq!(exec.opts.cmd, ["cargo", "test"]);
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
    fn exec_parses_or_native() {
        let cli = parse(&[
            "vm",
            "exec",
            "windows",
            "--or-native",
            "--",
            "cargo",
            "test",
        ]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert!(exec.opts.or_native);
        assert_eq!(exec.opts.cmd, ["cargo", "test"]);
    }

    #[test]
    fn exec_or_native_defaults_off() {
        let cli = parse(&["vm", "exec", "win", "--", "echo", "hi"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert!(!exec.opts.or_native);
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
    fn claude_takes_prompt_then_passthrough_args() {
        let cli = parse(&["vm", "claude", "lin", "fix the test", "--model", "sonnet"]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert_eq!(args.target, "lin");
        assert_eq!(args.prompt, "fix the test");
        assert_eq!(args.claude_args, ["--model", "sonnet"]);
        assert!(!args.with_snapshot);
        assert!(!args.no_writeback);
    }

    #[test]
    fn claude_own_flags_parse_before_the_prompt() {
        let cli = parse(&[
            "vm",
            "claude",
            "lin",
            "--with-snapshot",
            "--no-writeback",
            "do it",
        ]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert!(args.with_snapshot);
        assert!(args.no_writeback);
        assert_eq!(args.prompt, "do it");
        assert!(args.claude_args.is_empty());
    }

    #[test]
    fn claude_requires_a_prompt() {
        assert!(Cli::try_parse_from(["vm", "claude", "lin"]).is_err());
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

    #[test]
    fn guest_idle_verb_parses() {
        let cli = parse(&["vm", "_idle"]);
        assert!(matches!(cli.command, Command::GuestIdle));
    }

    #[test]
    fn guest_first_sync_verb_parses() {
        let cli = parse(&[
            "vm",
            "_first-sync",
            "--repo",
            "~/work/syncfs",
            "--cmd",
            "mise trust",
        ]);
        let Command::GuestFirstSync { repo, cmd } = cli.command else {
            panic!("expected _first-sync");
        };
        assert_eq!(repo, "~/work/syncfs");
        assert_eq!(cmd, "mise trust");
    }
}
