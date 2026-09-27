use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod config;
mod output;
mod self_update;
mod session;
mod whoami;

use output::{CliError, print_error};
use session::Session;

/// ter: fetch, build, run and check TER Learn exercises.
#[derive(Parser)]
#[command(name = "ter", version, about)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the account this machine is paired with, its courses and versions.
    Whoami,
    /// Update ter to the latest version.
    SelfUpdate,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command, cli.json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            print_error(&err, cli.json);
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command, json: bool) -> Result<(), CliError> {
    match command {
        Command::SelfUpdate => self_update::run(json),
        Command::Whoami => {
            let session = Session::open()?;
            let result = whoami::run(&session, json).await;
            notify_newer_version(&session, json);
            result
        }
    }
}

/// With `--json` the `latest_version` field says it instead.
fn notify_newer_version(session: &Session, json: bool) {
    if !json && let Some(notice) = session.newer_version_notice() {
        eprintln!("{notice}");
    }
}
