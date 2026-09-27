use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod config;
mod login;
mod output;
mod self_update;
mod session;
mod token_store;
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
    /// Pair this machine with your TER Learn account.
    Login {
        /// Print the approval link without trying to open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove this machine's device token.
    Logout,
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
        Command::Login { no_browser } => {
            let session = Session::open()?;
            login::login(&session, json, !no_browser).await
        }
        Command::Logout => login::logout(&Session::open()?, json),
        Command::Whoami => {
            let session = Session::open()?;
            let result = whoami::run(&session, json).await;
            notify_newer_version(&session, json);
            result.map_err(|e| session.forget_dead_token(e))
        }
    }
}

/// With `--json` the `latest_version` field says it instead.
fn notify_newer_version(session: &Session, json: bool) {
    if !json && let Some(notice) = session.newer_version_notice() {
        eprintln!("{notice}");
    }
}
