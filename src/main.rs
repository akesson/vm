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
            shell: opts.shell,
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
        Start { alias } => commands::start(&alias),
        Stop { alias, kill, force } => commands::stop(&alias, kill, force),
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
        Sync { alias } => commands::sync_cmd(&alias),

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

        Exec(args) => exec::host::exec(&args.target, &args.opts.into()),
        GuestExec => exec::guest::exec(),
        Deploy { alias } => deploy::deploy(&alias),
        Shot { alias, file } => commands::shot(&alias, file),
        WithSnapshot(args) => commands::with_snapshot(&args.target, &args.opts.into()),
        Claude(args) => vm::claude::run(
            &args.target,
            &vm::claude::ClaudeOptions {
                prompt: args.prompt,
                claude_args: args.claude_args,
                with_snapshot: args.with_snapshot,
                no_writeback: args.no_writeback,
            },
        ),
        Doctor { alias } => doctor::doctor(alias.as_deref()),
        Clean { alias } => commands::clean(&alias),
    }
}
