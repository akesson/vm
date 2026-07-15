mod cli;

use anyhow::Result;
use clap::Parser;
use vm::exec::host::ExecOptions;
use vm::{commands, deploy, doctor, exec, notice, proto, sync};

impl From<cli::ExecOpts> for ExecOptions {
    fn from(opts: cli::ExecOpts) -> ExecOptions {
        ExecOptions {
            no_sync: opts.no_sync,
            writeback: opts.writeback,
            with_snapshot: opts.with_snapshot,
            or_native: opts.or_native,
            guest_env: opts.guest_env,
            env: opts.env,
            with_file: opts.with_file,
            cmd: opts.cmd,
        }
    }
}

fn main() {
    let cli = cli::Cli::parse();
    // Only the host half of vm keeps a journal. The guest verbs below are the
    // agent, invoked inside a VM by a host `vm` over ssh or prlctl: their
    // stdout is a wire protocol the host parses back, and a log file written
    // inside a guest nobody shells into would be a log file nobody reads.
    // See `vm::journal`.
    if !is_guest_verb(&cli.command) {
        vm::journal::install_panic_hook();
        vm::journal::init(cli.quiet);
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // Every `Err` here is a vm-internal failure — a guest command's own
            // nonzero exit rides the `Ok(code)` path instead — so the only
            // question is whether it's the user's setup (usage, exit 2) or an
            // operational fault (infra, exit 125). See `vm::exit`.
            if err.downcast_ref::<vm::exit::UsageError>().is_some() {
                notice!("vm: config error: {err:#}");
                std::process::exit(vm::exit::USAGE);
            }
            notice!("vm: error: {err:#}");
            std::process::exit(vm::exit::INFRA);
        }
    }
}

/// The agent-side verbs — the ones a host `vm` invokes inside a guest, never a
/// human. Hidden in the CLI (`_exec`, `_tree`, …) and journal-free.
fn is_guest_verb(command: &cli::Command) -> bool {
    use cli::Command::*;
    matches!(
        command,
        GuestVersion
            | GuestSyncInit { .. }
            | GuestSyncApply { .. }
            | GuestTree { .. }
            | GuestFirstSync { .. }
            | GuestIdle
            | GuestExec
    )
}

/// Print the form advisories for a command line the caller typed, naming the
/// verb they typed it under so the suggested fix is one they can paste back.
fn advise(verb: &str, target: &str, cmd: &[String]) {
    for note in exec::advise::advisories(verb, target, cmd, |path| {
        std::path::Path::new(path).is_file()
    }) {
        notice!("vm ▸ note: {note}");
    }
}

fn run(cli: cli::Cli) -> Result<i32> {
    use cli::Command::*;
    match cli.command {
        Ls => commands::ls(),
        Reap {
            alias,
            idle_minutes,
            install,
            uninstall,
        } => match (install, uninstall) {
            (true, _) => vm::reap::install(idle_minutes),
            (_, true) => vm::reap::uninstall(),
            _ => vm::reap::reap(alias.as_deref(), idle_minutes),
        },
        Sync {
            alias,
            guest_env,
            with_file,
        } => commands::sync_cmd(&alias, guest_env, &with_file),

        // Guest-side plumbing verbs (invoked by a host `vm` over ssh)
        GuestVersion => {
            println!("{}", serde_json::to_string(&proto::VersionInfo::current())?);
            Ok(0)
        }
        GuestSyncInit { repo } => {
            sync::guest::ensure_init(&repo)?;
            Ok(0)
        }
        GuestSyncApply { repo, sha } => {
            println!("{}", sync::guest::apply(&repo, &sha)?);
            Ok(0)
        }
        GuestTree { repo } => {
            let snap = sync::guest::tree(&repo)?;
            println!("{}", serde_json::to_string(&snap)?);
            Ok(0)
        }
        GuestFirstSync { repo, cmd } => exec::guest::first_sync(&repo, &cmd),
        GuestIdle => {
            println!("{}", vm::idle::guest_idle_ms()?);
            Ok(0)
        }

        Exec(args) => {
            // Advisories are about what the *caller typed*, so they belong here
            // rather than in host::exec — which `vm claude` also drives, with an
            // argv it built itself and would only be confused about. Printed
            // before any VM work starts, so the note is on screen even if the
            // run then dies resuming the guest.
            advise("exec", &args.target, &args.opts.cmd);
            exec::host::exec(&args.target, &args.opts.into())
        }
        Run(args) => {
            // The arity rule is exec's, so its near-misses are too — and the
            // suggested fix has to come back as `vm run`, not `vm exec`.
            advise("run", &args.target, &args.cmd);
            exec::run::run(
                &args.target,
                &exec::run::RunOptions {
                    elevated: args.elevated,
                    env: args.env,
                    cmd: args.cmd,
                },
            )
        }
        GuestExec => exec::guest::exec(),
        Deploy { alias } => deploy::deploy(&alias),
        Shot { alias, file } => commands::shot(&alias, file),
        Claude(args) => vm::claude::run(
            &args.target,
            &vm::claude::ClaudeOptions {
                prompt: args.prompt,
                claude_args: args.claude_args,
                with_snapshot: args.with_snapshot,
                no_writeback: args.no_writeback,
                env: args.env,
                with_file: args.with_file,
                guest_env: args.guest_env,
            },
        ),
        Doctor { alias } => doctor::doctor(alias.as_deref()),
        Clean { alias } => commands::clean(&alias),
    }
}
