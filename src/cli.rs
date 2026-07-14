use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use vm::guest_env::GuestEnv;

/// Run commands in Parallels VMs against a synced copy of the current repo.
///
/// The host working tree is the single source of truth. Before every exec,
/// the repo is snapshotted (dirty state included, staging area untouched)
/// and pushed to a per-guest native checkout, so guests see exactly what git
/// sees on the host: uncommitted and untracked files included, gitignored
/// files excluded. Builds run on guest-local disk: no shared folders, no
/// cross-platform artifact conflicts.
///
/// VM lifecycle takes care of itself: any command that needs a guest starts it
/// first (a boot takes seconds, and vm says what it is waiting for), and
/// `vm reap` shuts down VMs nobody is using. There is nothing to start or stop
/// by hand.
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
    /// Sync the current repo to a guest, then run a command in the guest checkout
    ///
    /// The sync carries uncommitted and untracked files; gitignored files (build
    /// caches, `.env`…) stay on the host unless named with --with-file.
    ///
    /// Exit status is the guest command's own, except vm's reserved codes:
    /// 125 = vm infrastructure error (sync/agent/ssh/VM lifecycle), 2 =
    /// usage/config error. See the README's "Exit codes" section.
    Exec(ExecArgs),
    /// Run a command in a guest with no repo and no sync — the guest itself is
    /// the subject: patch it, install a tool, ask what version it has
    ///
    /// Runs in the guest user's home directory. Needs no git repo, so it works
    /// from anywhere. Same command form as exec: SEVERAL arguments run exactly
    /// as given; a SINGLE argument is a script for the guest's own shell.
    ///
    /// Unlike exec, input piped or redirected into vm IS sent to the guest
    /// command's stdin (up to 8 MiB of text) — which is how a script gets in:
    /// `vm run linux --elevated -- sh < step.sh`.
    Run(RunArgs),
    /// Sync the current repo to a guest without running anything
    ///
    /// Carries uncommitted and untracked files; gitignored files (build caches,
    /// `.env`…) stay on the host unless named with --with-file.
    Sync {
        /// VM alias from ~/.config/vm/config.toml
        alias: String,
        /// Guest environment handling: `mise` forces the mise setup, `none`
        /// disables it. Default: auto-detect from the repo root
        #[arg(long, value_enum, value_name = "ENV")]
        guest_env: Option<GuestEnv>,
        /// Sync a gitignored file too (e.g. `--with-file .env`). Repeatable.
        /// It stays in the guest checkout until a sync that does not name it
        #[arg(long, value_name = "PATH")]
        with_file: Vec<String>,
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
    /// Run Claude Code headless in the guest checkout of the current repo
    ///
    /// Syncs the repo, runs `claude -p` with permission prompts bypassed —
    /// the VM is the permission boundary — and applies source edits back to
    /// the host tree (writeback) when it finishes. Requires the claude CLI
    /// installed and authenticated in the guest (`vm doctor` verifies).
    Claude(ClaudeArgs),
    /// Shut down VMs that no vm process is using and that have been idle a
    /// while (any `vm exec` boots them again)
    Reap {
        /// Only consider this VM (default: all configured VMs)
        alias: Option<String>,
        /// Idle threshold in minutes, measured from the end of the last use
        #[arg(long, default_value_t = 30)]
        idle_minutes: u64,
        /// Install a launchd job running `vm reap` every 5 minutes; the job
        /// bakes in this invocation's --idle-minutes
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
        /// The first-sync setup command (e.g. `mise trust` from the mise
        /// guest env)
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
    /// VM alias from ~/.config/vm/config.toml. With --or-native, a target
    /// literally named windows | linux | macos that matches the host OS runs
    /// natively without needing the config (CI runners)
    pub target: String,
    #[command(flatten)]
    pub opts: ExecOpts,
}

#[derive(Args)]
pub struct RunArgs {
    /// VM alias from ~/.config/vm/config.toml
    pub target: String,
    /// Run as the superuser: root on linux/macos, SYSTEM on windows, via
    /// Parallels Tools. The only elevation available — sudo over ssh wants a
    /// password, and the Windows guest user is not an administrator. Needs no
    /// console login. Note the superuser's PATH is the system one: per-user
    /// tools (mise, cargo, a user-scope brew) are not on it
    #[arg(long)]
    pub elevated: bool,
    /// Set an env var for the guest command: `-e NAME=value`, or `-e NAME`
    /// to forward the host's current value. Repeatable.
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
    /// Command to run in the guest user's home. SEVERAL arguments run exactly as
    /// given, byte for byte, with no shell involved. A SINGLE argument is run as
    /// a script by the guest's own shell (`sh -c`, or `cmd /C` on Windows).
    /// Input piped or redirected into vm becomes the command's stdin, so a
    /// script goes in as `vm run <alias> -- sh < step.sh`
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Args)]
pub struct ClaudeArgs {
    /// VM alias from ~/.config/vm/config.toml
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
    /// Set an env var for the guest claude process: `-e NAME=value`, or
    /// `-e NAME` to forward the host's current value. Repeatable; must come
    /// before the prompt
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
    /// Sync a gitignored file into the guest checkout too (e.g.
    /// `--with-file .env`), so the agent's build sees it. Repeatable; must come
    /// before the prompt
    #[arg(long, value_name = "PATH")]
    pub with_file: Vec<String>,
    /// Guest environment handling: `mise` forces the mise setup/wrap, `none`
    /// disables it. Default: auto-detect from the repo root
    #[arg(long, value_enum, value_name = "ENV")]
    pub guest_env: Option<GuestEnv>,
    /// Extra arguments passed to claude verbatim (e.g. --model sonnet).
    /// Everything from the first argument vm does not know onwards goes to
    /// claude — so vm's own flags must come before the prompt
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub claude_args: Vec<String>,
}

#[derive(Args)]
pub struct ExecOpts {
    /// Snapshot the VM first, run the command, then roll the VM back — the
    /// guest keeps nothing from the run (needs the VM to itself and ~2× the
    /// VM's RAM free on disk)
    #[arg(long, conflicts_with = "or_native")]
    pub with_snapshot: bool,
    /// Skip the pre-exec sync (run against the guest checkout as-is)
    #[arg(long)]
    pub no_sync: bool,
    /// After the command, apply source changes made in the guest back to the
    /// host tree. Cannot be combined with --no-sync: the guest diff is measured
    /// against the commit the sync pushes, so with no sync there is no base
    #[arg(long, conflicts_with = "no_sync")]
    pub writeback: bool,
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
    /// Sync a gitignored file into the guest checkout too (e.g.
    /// `--with-file .env`) — the sync otherwise carries only what git sees.
    /// Repeatable. It stays in the checkout until a sync that does not name it,
    /// and its contents do land on the guest's disk (`-e NAME=value` passes a
    /// value without that)
    #[arg(long, value_name = "PATH", conflicts_with = "no_sync")]
    pub with_file: Vec<String>,
    /// Guest environment handling: `mise` forces the mise setup/wrap, `none`
    /// disables it. Default: auto-detect from the repo root (announced by a
    /// breadcrumb when active)
    #[arg(long, value_enum, value_name = "ENV")]
    pub guest_env: Option<GuestEnv>,
    /// Command to run in the guest checkout. SEVERAL arguments run exactly as
    /// given, byte for byte, with no shell involved. A SINGLE argument is run as
    /// a script by the guest's own shell (`sh -c`, or `cmd /C` on Windows), so
    /// `vm exec <alias> -- 'cd src && cargo test'` gets pipes, `&&` and builtins.
    ///
    /// To run a script you wrote, put it in a file and name it — the sync
    /// carries untracked files, so `vm exec <alias> -- python3 script.py` just
    /// works. Piping it in does NOT: exec never forwards its own stdin, and the
    /// command would run with no input at all (`vm run` is the one that feeds
    /// stdin to the guest)
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("should parse")
    }

    /// `vm start` and `vm stop` are gone. Every command that needs a guest brings
    /// it up itself, and `vm reap` shuts down the ones nobody is using — so the two
    /// verbs did nothing a caller had to do, while implying they were a step one
    /// had to remember. Pinned so they cannot quietly come back: a `start` in the
    /// help text is an instruction to use it.
    #[test]
    fn there_are_no_lifecycle_verbs() {
        assert!(Cli::try_parse_from(["vm", "start", "lin"]).is_err());
        assert!(Cli::try_parse_from(["vm", "stop", "lin"]).is_err());
    }

    #[test]
    fn exec_keeps_hyphen_args_after_command() {
        let cli = parse(&["vm", "exec", "win", "--", "cargo", "build", "--release"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.target, "win");
        assert_eq!(exec.opts.cmd, ["cargo", "build", "--release"]);
    }

    #[test]
    fn a_single_trailing_argument_survives_as_one() {
        // The arity rule reads this as a script, so clap must not have split it
        // — everything downstream keys off `cmd.len() == 1`.
        let cli = parse(&["vm", "exec", "lin", "--", "cd src && cargo test"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.cmd, ["cd src && cargo test"]);
    }

    #[test]
    fn a_leftover_shell_flag_lands_in_the_command() {
        // `cmd` is trailing_var_arg, so clap does not reject the removed flag —
        // it swallows it. Pinned here because that is *why* exec::host rejects a
        // command starting with `--shell` by hand: left alone, an old script
        // would go hunting the guest for a binary named `--shell`.
        let cli = parse(&["vm", "exec", "lin", "--shell", "--", "echo hi"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.cmd[0], "--shell");
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
    fn exec_parses_with_snapshot() {
        let cli = parse(&[
            "vm",
            "exec",
            "win",
            "--with-snapshot",
            "--",
            "cargo",
            "test",
        ]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert!(exec.opts.with_snapshot);
        assert_eq!(exec.opts.cmd, ["cargo", "test"]);
    }

    #[test]
    fn with_snapshot_conflicts_with_or_native_at_parse_time() {
        // The host cannot be snapshotted, so the combination is rejected by
        // clap itself instead of surfacing later at runtime.
        assert!(
            Cli::try_parse_from([
                "vm",
                "exec",
                "win",
                "--with-snapshot",
                "--or-native",
                "--",
                "true",
            ])
            .is_err()
        );
    }

    #[test]
    fn writeback_conflicts_with_no_sync_at_parse_time() {
        // Writeback diffs the guest tree against the commit the sync pushed, so
        // with --no-sync there is no base to diff against and the writeback
        // could only be silently skipped. Reject it up front instead.
        assert!(
            Cli::try_parse_from([
                "vm",
                "exec",
                "lin",
                "--no-sync",
                "--writeback",
                "--",
                "true"
            ])
            .is_err()
        );
    }

    #[test]
    fn exec_collects_repeated_with_file_flags() {
        let cli = parse(&[
            "vm",
            "exec",
            "lin",
            "--with-file",
            ".env",
            "--with-file",
            "config/local.toml",
            "--",
            "cargo",
            "test",
        ]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.with_file, [".env", "config/local.toml"]);
        assert_eq!(exec.opts.cmd, ["cargo", "test"]);
    }

    #[test]
    fn with_file_conflicts_with_no_sync_at_parse_time() {
        // The file rides the sync's snapshot; with no sync there is nothing for
        // it to ride, and it could only be silently ignored.
        assert!(
            Cli::try_parse_from([
                "vm",
                "exec",
                "lin",
                "--no-sync",
                "--with-file",
                ".env",
                "--",
                "true",
            ])
            .is_err()
        );
    }

    #[test]
    fn sync_and_claude_take_with_file_too() {
        // The flag has to exist wherever a sync happens, or a caller who hit the
        // note on `vm exec` cannot act on it from `vm claude`.
        let cli = parse(&["vm", "sync", "lin", "--with-file", ".env"]);
        let Command::Sync { with_file, .. } = cli.command else {
            panic!("expected sync");
        };
        assert_eq!(with_file, [".env"]);

        let cli = parse(&[
            "vm",
            "claude",
            "lin",
            "--with-file",
            ".env",
            "fix the build",
        ]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert_eq!(args.with_file, [".env"]);
        assert_eq!(args.prompt, "fix the build");
    }

    #[test]
    fn exec_parses_guest_env_choices() {
        let cli = parse(&["vm", "exec", "win", "--guest-env", "none", "--", "true"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.guest_env, Some(GuestEnv::None));

        let cli = parse(&["vm", "exec", "win", "--guest-env", "mise", "--", "true"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.guest_env, Some(GuestEnv::Mise));
    }

    #[test]
    fn exec_guest_env_defaults_to_auto_detect() {
        let cli = parse(&["vm", "exec", "win", "--", "true"]);
        let Command::Exec(exec) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(exec.opts.guest_env, None);
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

    // ── vm run ────────────────────────────────────────────────────────────────

    #[test]
    fn run_parses_elevated_and_env_before_the_command() {
        let cli = parse(&[
            "vm",
            "run",
            "lin",
            "--elevated",
            "-e",
            "FOO=bar",
            "--",
            "apt-get",
            "update",
        ]);
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.target, "lin");
        assert!(run.elevated);
        assert_eq!(run.env, ["FOO=bar"]);
        assert_eq!(run.cmd, ["apt-get", "update"]);
    }

    #[test]
    fn run_elevated_defaults_off() {
        let cli = parse(&["vm", "run", "lin", "--", "whoami"]);
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert!(!run.elevated, "elevation is never implicit");
    }

    #[test]
    fn run_keeps_hyphen_args_and_a_lone_script_intact() {
        let cli = parse(&["vm", "run", "win", "--", "winget", "upgrade", "--all"]);
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.cmd, ["winget", "upgrade", "--all"]);

        // The arity rule reads one argument as a script, so clap must not split it.
        let cli = parse(&["vm", "run", "lin", "--", "id -u && uname -a"]);
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.cmd, ["id -u && uname -a"]);
    }

    /// run is sync-less by construction, so exec's repo flags do not exist on
    /// it — but `cmd` is trailing_var_arg, so clap does not *reject* one, it
    /// swallows it into the command. Pinned here because that is why
    /// `exec::run::reject_exec_flags` exists: left alone, the guest would go
    /// hunting for a binary called `--no-sync` and come back with a 127.
    #[test]
    fn an_exec_only_flag_lands_in_runs_command_rather_than_failing_to_parse() {
        let cli = parse(&["vm", "run", "lin", "--no-sync", "--", "true"]);
        let Command::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.cmd[0], "--no-sync");
    }

    #[test]
    fn run_requires_a_target_and_a_command() {
        assert!(Cli::try_parse_from(["vm", "run"]).is_err());
        assert!(Cli::try_parse_from(["vm", "run", "lin"]).is_err());
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
    fn claude_env_flags_parse_before_the_prompt() {
        let cli = parse(&[
            "vm", "claude", "lin", "-e", "FOO=bar", "--env", "BAZ", "do it",
        ]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert_eq!(args.env, ["FOO=bar", "BAZ"]);
        assert_eq!(args.prompt, "do it");
        assert!(args.claude_args.is_empty());
    }

    /// The passthrough tail starts at the first argument vm does not know, and
    /// swallows everything after it — including names vm *does* know. So a vm
    /// flag reaches vm before `--model`, and claude after it. `vm::claude`
    /// rejects the second shape rather than letting the flag quietly vanish;
    /// this pins the parse behavior that makes that check necessary.
    #[test]
    fn a_vm_flag_after_an_unknown_flag_lands_in_the_passthrough_tail() {
        let cli = parse(&["vm", "claude", "lin", "do it", "--no-writeback"]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert!(args.no_writeback, "before the tail, it is vm's own flag");
        assert!(args.claude_args.is_empty());

        let cli = parse(&[
            "vm",
            "claude",
            "lin",
            "do it",
            "--model",
            "sonnet",
            "--no-writeback",
        ]);
        let Command::Claude(args) = cli.command else {
            panic!("expected claude");
        };
        assert!(!args.no_writeback, "inside the tail, vm never sees it");
        assert_eq!(args.claude_args, ["--model", "sonnet", "--no-writeback"]);
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
