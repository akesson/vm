mod cli;
// The allows below cover items that get wired up in phases 2–4; drop them as verbs land.
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod mapping;
#[allow(dead_code)]
mod proto;

use anyhow::{Result, bail};
use clap::Parser;

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
        GuestVersion => {
            println!("{}", serde_json::to_string(&proto::VersionInfo::current())?);
            Ok(0)
        }
        Ls => bail!("`vm ls` lands in phase 2"),
        Start { .. } | Stop { .. } | Suspend { .. } => bail!("lifecycle verbs land in phase 2"),
        Sync { .. } | GuestSyncApply { .. } | GuestTree { .. } => {
            bail!("sync engine lands in phase 3")
        }
        Exec(_) | Run(_) | GuestExec | Deploy { .. } => bail!("exec agent lands in phase 4"),
        Shot { .. } | WithSnapshot(_) | Doctor { .. } | Clean { .. } => {
            bail!("this verb lands in phase 7")
        }
    }
}
