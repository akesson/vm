//! `vm claude <target> <prompt>` and `vm codex <target> <prompt>` — run a
//! coding agent headless in a guest checkout.
//!
//! The VM is the permission boundary: the agent runs with every confirmation
//! turned off, free to execute anything inside the guest, while the host tree
//! only ever receives the explicit writeback diff (source changes; build
//! artifacts and other guest state stay put). Combine with `--with-snapshot`
//! and the guest itself is rolled back too, so a run leaves nothing behind but
//! the diff.
//!
//! The two agents differ in the binary they run and the argv that puts it in
//! headless mode, and in nothing else vm cares about — so they share every line
//! here rather than existing as two modules that drift apart. A third agent is
//! one [`Agent`] arm.

use crate::exec::host::ExecOptions;
use crate::exit::usage;
use crate::guest_env::GuestEnv;
use anyhow::Result;

/// A coding agent vm can drive headless in a guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// The vm subcommand that selects this agent, which is deliberately also
    /// the guest binary it runs: `vm codex` runs `codex`.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// The argv between the binary and the caller's passthrough args: run
    /// non-interactively, and skip every confirmation — the VM is the
    /// permission boundary, so an agent stopping to ask inside it would only be
    /// a run that hangs with nobody there to answer.
    ///
    /// Measured against Claude Code 2.1.278 and codex-cli 0.155.0. The two
    /// headless forms are not spelled alike: claude takes a flag (`-p`), codex
    /// takes a subcommand (`exec`).
    fn headless(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["-p", "--dangerously-skip-permissions"],
            Agent::Codex => &["exec", "--dangerously-bypass-approvals-and-sandbox"],
        }
    }

    /// A call that proves the guest's credentials are *live*, for `vm doctor`.
    /// Run from the guest's home directory, which is not a repo — hence codex's
    /// `--skip-git-repo-check`, without which it refuses to start at all.
    ///
    /// claude's pins the cheapest model on purpose. codex's does not: naming a
    /// model the account cannot reach would make doctor report a login problem
    /// that is nothing of the kind, and a false alarm costs more than a call.
    pub fn auth_probe(self) -> &'static str {
        match self {
            Agent::Claude => r#"claude -p --model haiku "say hi""#,
            Agent::Codex => r#"codex exec --skip-git-repo-check "say hi""#,
        }
    }

    /// What to tell someone whose credentials the probe found stale.
    pub fn login_hint(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex login",
        }
    }
}

pub struct AgentOptions {
    pub agent: Agent,
    pub prompt: String,
    /// Extra arguments passed to the agent verbatim, before the prompt.
    pub agent_args: Vec<String>,
    pub with_snapshot: bool,
    pub no_writeback: bool,
    /// `-e` specs forwarded to the guest agent process.
    pub env: Vec<String>,
    /// `--with-file` paths: gitignored files to sync into the checkout the agent
    /// works in.
    pub with_file: Vec<String>,
    pub guest_env: Option<GuestEnv>,
}

/// vm's own flags on `vm claude` / `vm codex`. Once clap starts filling the
/// verbatim passthrough tail (at the first arg it does not know, e.g.
/// `--model`), every later arg lands there raw — so `--no-writeback` *before*
/// `--model` reaches vm, and *after* it silently reaches the agent instead. See
/// [`reject_misplaced_vm_flags`].
const VM_FLAGS: &[&str] = &[
    "--with-snapshot",
    "--no-writeback",
    "--guest-env",
    "--with-file",
    "--quiet",
    "-q",
];

pub fn run(target: &str, opts: &AgentOptions) -> Result<i32> {
    reject_misplaced_vm_flags(opts.agent, &opts.agent_args)?;
    let exec = ExecOptions {
        no_sync: false,
        writeback: !opts.no_writeback,
        with_snapshot: opts.with_snapshot,
        // The agent is the thing being contained — it always runs in the VM.
        or_native: false,
        // In a mise repo the agent runs under `mise exec --`, so the commands
        // *it* spawns in the guest resolve the repo's tools.
        guest_env: opts.guest_env,
        env: opts.env.clone(),
        with_file: opts.with_file.clone(),
        cmd: argv(opts),
    };
    crate::exec::host::exec(target, &exec)
}

/// A vm flag that landed in the passthrough tail would be handed to the agent,
/// which does not have it — so the flag the caller *did* pass would take no
/// effect here. Refuse rather than warn: the flags in question are the ones
/// that hold vm back from touching things (`--no-writeback` keeps the host tree
/// untouched), and a silently dropped safety flag is exactly the failure worth
/// paying an exit-2 for. The agent's own flags are unaffected — only the names
/// in [`VM_FLAGS`] are reserved.
fn reject_misplaced_vm_flags(agent: Agent, agent_args: &[String]) -> Result<()> {
    let tool = agent.name();
    for arg in agent_args {
        let name = arg.split('=').next().unwrap_or(arg);
        if VM_FLAGS.contains(&name) {
            return Err(usage(format!(
                "`{name}` is a vm flag, but here it sits after an argument vm does not know, \
                 so it would be passed to {tool} verbatim and have no effect.\n  \
                 Put vm's own flags before the prompt: \
                 `vm {tool} <alias> {name} … \"<prompt>\" [{tool} flags…]`"
            )));
        }
    }
    Ok(())
}

fn argv(opts: &AgentOptions) -> Vec<String> {
    let mut argv = vec![opts.agent.name().to_string()];
    argv.extend(opts.agent.headless().iter().map(|s| s.to_string()));
    argv.extend(opts.agent_args.iter().cloned());
    argv.push(opts.prompt.clone());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(agent: Agent, agent_args: &[&str]) -> AgentOptions {
        AgentOptions {
            agent,
            prompt: "fix the failing test".into(),
            agent_args: agent_args.iter().map(|s| s.to_string()).collect(),
            with_snapshot: false,
            no_writeback: false,
            env: Vec::new(),
            with_file: Vec::new(),
            guest_env: None,
        }
    }

    #[test]
    fn argv_puts_extra_args_before_the_prompt() {
        assert_eq!(
            argv(&opts(Agent::Claude, &["--model", "sonnet"])),
            [
                "claude",
                "-p",
                "--dangerously-skip-permissions",
                "--model",
                "sonnet",
                "fix the failing test",
            ]
        );
    }

    /// codex's headless mode is a *subcommand*, not a flag, so the order here is
    /// load-bearing in a way claude's is not: `codex --dangerously-… exec` is a
    /// different command line, and `codex "<prompt>"` with no `exec` at all
    /// opens the interactive TUI against a guest nobody is looking at.
    #[test]
    fn codex_runs_the_exec_subcommand_with_approvals_bypassed() {
        assert_eq!(
            argv(&opts(Agent::Codex, &["--model", "gpt-5.1-codex"])),
            [
                "codex",
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5.1-codex",
                "fix the failing test",
            ]
        );
    }

    /// The prompt is the last element for both, and one element — a prompt that
    /// looks like a flag is still a prompt.
    #[test]
    fn the_prompt_stays_one_trailing_argument() {
        for agent in [Agent::Claude, Agent::Codex] {
            let argv = argv(&opts(agent, &[]));
            assert_eq!(argv.last().unwrap(), "fix the failing test");
            assert_eq!(argv[0], agent.name());
        }
    }

    fn reject(agent: Agent, agent_args: &[&str]) -> Result<()> {
        let args: Vec<String> = agent_args.iter().map(|s| s.to_string()).collect();
        reject_misplaced_vm_flags(agent, &args)
    }

    #[test]
    fn a_vm_flag_in_the_passthrough_tail_is_rejected() {
        // `vm claude lin "p" --model sonnet --no-writeback`: clap fills the tail
        // from `--model` on, so --no-writeback would reach claude and vm would
        // write back anyway — refuse instead of quietly doing the wrong thing.
        let err = reject(Agent::Claude, &["--model", "sonnet", "--no-writeback"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--no-writeback"), "{err}");
        assert!(err.contains("before the prompt"), "{err}");
    }

    /// The same refusal, and the fix it suggests has to be a line the caller can
    /// paste back — under the verb they actually typed.
    #[test]
    fn the_refusal_names_the_agent_that_was_asked_for() {
        let err = reject(
            Agent::Codex,
            &["--model", "gpt-5.1-codex", "--with-snapshot"],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("vm codex <alias>"), "{err}");
        assert!(!err.contains("claude"), "{err}");
    }

    #[test]
    fn the_value_form_is_rejected_too() {
        assert!(reject(Agent::Claude, &["--verbose", "--guest-env=none"]).is_err());
    }

    #[test]
    fn the_agents_own_flags_pass_through_untouched() {
        assert!(reject(Agent::Claude, &["--model", "sonnet", "--verbose"]).is_ok());
        // codex's own near-miss: `--skip-git-repo-check` is not vm's flag, and
        // `-c key=value` is codex config, not vm config.
        assert!(reject(Agent::Codex, &["--skip-git-repo-check", "-c", "model=x"]).is_ok());
    }

    #[test]
    fn a_misplaced_vm_flag_is_a_usage_error_not_an_infra_one() {
        let err = reject(Agent::Claude, &["--verbose", "--with-snapshot"]).unwrap_err();
        assert!(err.downcast_ref::<crate::exit::UsageError>().is_some());
    }
}
