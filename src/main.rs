mod cli;

use anyhow::Result;
use clap::Parser;
use vm::exec::host::ExecOptions;
use vm::{commands, deploy, doctor, exec, proto, sync};

impl From<cli::ExecOpts> for ExecOptions {
    fn from(opts: cli::ExecOpts) -> ExecOptions {
        ExecOptions {
            no_sync: opts.no_sync,
            writeback: opts.writeback,
            with_snapshot: opts.with_snapshot,
            or_native: opts.or_native,
            guest_env: opts.guest_env,
            env: opts.env,
            cmd: opts.cmd,
        }
    }
}

fn main() {
    let cli = cli::Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // Every `Err` here is a vm-internal failure — a guest command's own
            // nonzero exit rides the `Ok(code)` path instead — so the only
            // question is whether it's the user's setup (usage, exit 2) or an
            // operational fault (infra, exit 125). See `vm::exit`.
            if err.downcast_ref::<vm::exit::UsageError>().is_some() {
                eprintln!("vm: config error: {err:#}");
                std::process::exit(vm::exit::USAGE);
            }
            eprintln!("vm: error: {err:#}");
            std::process::exit(vm::exit::INFRA);
        }
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
        Sync { alias, guest_env } => commands::sync_cmd(&alias, guest_env),

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
            for note in exec::advise::advisories(&args.target, &args.opts.cmd, |path| {
                std::path::Path::new(path).is_file()
            }) {
                eprintln!("vm ▸ note: {note}");
            }
            exec::host::exec(&args.target, &args.opts.into())
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
                guest_env: args.guest_env,
            },
        ),
        Doctor { alias } => doctor::doctor(alias.as_deref()),
        Clean { alias } => commands::clean(&alias),
    }
}
