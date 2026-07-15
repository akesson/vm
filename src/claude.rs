//! `vm claude <target> <prompt>` — run Claude Code headless in a guest
//! checkout.
//!
//! The VM is the permission boundary: the agent runs with
//! `--dangerously-skip-permissions`, free to execute anything inside the
//! guest, while the host tree only ever receives the explicit writeback
//! diff (source changes; build artifacts and other guest state stay put).
//! Combine with `--with-snapshot` and the guest itself is rolled back too,
//! so a run leaves nothing behind but the diff.

use crate::exec::host::ExecOptions;
use crate::exit::usage;
use crate::guest_env::GuestEnv;
use anyhow::Result;

pub struct ClaudeOptions {
    pub prompt: String,
    /// Extra arguments passed to claude verbatim, before the prompt.
    pub claude_args: Vec<String>,
    pub with_snapshot: bool,
    pub no_writeback: bool,
    /// `-e` specs forwarded to the guest claude process.
    pub env: Vec<String>,
    /// `--with-file` paths: gitignored files to sync into the checkout the agent
    /// works in.
    pub with_file: Vec<String>,
    pub guest_env: Option<GuestEnv>,
}

/// vm's own flags on `vm claude`. Once clap starts filling the verbatim
/// passthrough tail (at the first arg it does not know, e.g. `--model`), every
/// later arg lands there raw — so `--no-writeback` *before* `--model` reaches
/// vm, and *after* it silently reaches claude instead. See
/// [`reject_misplaced_vm_flags`].
const VM_FLAGS: &[&str] = &[
    "--with-snapshot",
    "--no-writeback",
    "--guest-env",
    "--with-file",
    "--quiet",
    "-q",
];

pub fn run(target: &str, opts: &ClaudeOptions) -> Result<i32> {
    reject_misplaced_vm_flags(&opts.claude_args)?;
    let exec = ExecOptions {
        no_sync: false,
        writeback: !opts.no_writeback,
        with_snapshot: opts.with_snapshot,
        // claude is the permission boundary — it always runs in the VM.
        or_native: false,
        // In a mise repo claude runs under `mise exec --`, so the commands *it*
        // spawns in the guest resolve the repo's tools.
        guest_env: opts.guest_env,
        env: opts.env.clone(),
        with_file: opts.with_file.clone(),
        cmd: argv(opts),
    };
    crate::exec::host::exec(target, &exec)
}

/// A vm flag that landed in the passthrough tail would be handed to claude,
/// which does not have it — so the flag the caller *did* pass would take no
/// effect here. Refuse rather than warn: the flags in question are the ones
/// that hold vm back from touching things (`--no-writeback` keeps the host tree
/// untouched), and a silently dropped safety flag is exactly the failure worth
/// paying an exit-2 for. Claude's own flags are unaffected — only the names in
/// [`VM_FLAGS`] are reserved.
fn reject_misplaced_vm_flags(claude_args: &[String]) -> Result<()> {
    for arg in claude_args {
        let name = arg.split('=').next().unwrap_or(arg);
        if VM_FLAGS.contains(&name) {
            return Err(usage(format!(
                "`{name}` is a vm flag, but here it sits after an argument vm does not know, \
                 so it would be passed to claude verbatim and have no effect.\n  \
                 Put vm's own flags before the prompt: \
                 `vm claude <alias> {name} … \"<prompt>\" [claude flags…]`"
            )));
        }
    }
    Ok(())
}

fn argv(opts: &ClaudeOptions) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    argv.extend(opts.claude_args.iter().cloned());
    argv.push(opts.prompt.clone());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(claude_args: &[&str]) -> ClaudeOptions {
        ClaudeOptions {
            prompt: "fix the failing test".into(),
            claude_args: claude_args.iter().map(|s| s.to_string()).collect(),
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
            argv(&opts(&["--model", "sonnet"])),
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

    fn reject(claude_args: &[&str]) -> Result<()> {
        let args: Vec<String> = claude_args.iter().map(|s| s.to_string()).collect();
        reject_misplaced_vm_flags(&args)
    }

    #[test]
    fn a_vm_flag_in_the_passthrough_tail_is_rejected() {
        // `vm claude lin "p" --model sonnet --no-writeback`: clap fills the tail
        // from `--model` on, so --no-writeback would reach claude and vm would
        // write back anyway — refuse instead of quietly doing the wrong thing.
        let err = reject(&["--model", "sonnet", "--no-writeback"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--no-writeback"), "{err}");
        assert!(err.contains("before the prompt"), "{err}");
    }

    #[test]
    fn the_value_form_is_rejected_too() {
        assert!(reject(&["--verbose", "--guest-env=none"]).is_err());
    }

    #[test]
    fn claudes_own_flags_pass_through_untouched() {
        assert!(reject(&["--model", "sonnet", "--verbose"]).is_ok());
    }

    #[test]
    fn a_misplaced_vm_flag_is_a_usage_error_not_an_infra_one() {
        let err = reject(&["--verbose", "--with-snapshot"]).unwrap_err();
        assert!(err.downcast_ref::<crate::exit::UsageError>().is_some());
    }
}
