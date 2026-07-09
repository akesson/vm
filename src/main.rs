mod cli;

use anyhow::{Result, bail};
use clap::Parser;
use vm::{commands, proto, sync};

fn main() {
    let cli = cli::Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("vm: error: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: cli::Cli) -> Result<i32> {
    use cli::Command::*;
    match cli.command {
        Ls => commands::ls(),
        Start { alias } => commands::start(&alias),
        Stop { alias, kill } => commands::stop(&alias, kill),
        Suspend { alias } => commands::suspend(&alias),
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

        Exec(_) | Run(_) | GuestExec | Deploy { .. } => bail!("exec agent lands in phase 4"),
        Shot { .. } | WithSnapshot(_) | Doctor { .. } | Clean { .. } => {
            bail!("this verb lands in phase 7")
        }
    }
}
