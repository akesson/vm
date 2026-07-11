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
use anyhow::Result;

pub struct ClaudeOptions {
    pub prompt: String,
    /// Extra arguments passed to claude verbatim, before the prompt.
    pub claude_args: Vec<String>,
    pub with_snapshot: bool,
    pub no_writeback: bool,
}

pub fn run(target: &str, opts: &ClaudeOptions) -> Result<i32> {
    let exec = ExecOptions {
        no_sync: false,
        writeback: !opts.no_writeback,
        shell: false,
        // claude is the permission boundary — it always runs in the VM, and its
        // own `claude` argv is never subject to the repo's `wrap` prefix.
        or_native: false,
        apply_wrap: false,
        env: Vec::new(),
        cmd: argv(opts),
    };
    if opts.with_snapshot {
        crate::commands::with_snapshot(target, &exec)
    } else {
        crate::exec::host::exec(target, &exec)
    }
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

    #[test]
    fn argv_puts_extra_args_before_the_prompt() {
        let opts = ClaudeOptions {
            prompt: "fix the failing test".into(),
            claude_args: vec!["--model".into(), "sonnet".into()],
            with_snapshot: false,
            no_writeback: false,
        };
        assert_eq!(
            argv(&opts),
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
}
