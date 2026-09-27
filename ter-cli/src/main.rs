use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod config;
mod exercises;
mod login;
mod output;
mod self_update;
mod session;
mod token_store;
mod whoami;

use exercises::CleanScope;
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
    /// Exercises: list, fetch, clean and remove.
    Ex {
        #[command(subcommand)]
        command: ExCommand,
    },
    /// Update ter to the latest version.
    SelfUpdate,
}

#[derive(Subcommand)]
enum ExCommand {
    /// List the exercises in your courses.
    List {
        /// Only this course (its id, as `ter ex list` shows it).
        #[arg(long)]
        course: Option<String>,
    },
    /// Fetch an exercise into its course folder, as a Cargo project.
    Fetch {
        /// The exercise id, as `ter ex list` shows it.
        id: String,
        /// Put it here instead of <courses-root>/<course>/<id>.
        dir: Option<PathBuf>,
        /// Read it from a local curriculum checkout instead of the site.
        #[arg(long, value_name = "CURRICULUM_PATH")]
        dev: Option<PathBuf>,
    },
    /// Delete build output and run recordings; the source stays. With no
    /// argument, the exercise in the current folder.
    Clean {
        /// One exercise.
        #[arg(conflicts_with_all = ["course", "all"])]
        id: Option<String>,
        /// A course: its shared build cache and every exercise's output.
        #[arg(long, conflicts_with = "all")]
        course: Option<String>,
        /// Every course.
        #[arg(long)]
        all: bool,
    },
    /// Delete an exercise's folder. Refused if you have edited it, unless
    /// --force.
    Remove {
        id: String,
        /// Delete it even though it has changes.
        #[arg(long)]
        force: bool,
    },
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
        Command::Ex { command } => ex(command, json).await,
        Command::Whoami => {
            let session = Session::open()?;
            let result = whoami::run(&session, json).await;
            notify_newer_version(&session, json);
            result.map_err(|e| session.forget_dead_token(e))
        }
    }
}

async fn ex(command: ExCommand, json: bool) -> Result<(), CliError> {
    match command {
        ExCommand::List { course } => {
            let session = Session::open()?;
            exercises::list(&session, course.as_deref(), json)
                .await
                .map_err(|e| session.forget_dead_token(e))
        }
        ExCommand::Fetch { id, dir, dev } => exercises::fetch(&id, dir, dev.as_deref(), json).await,
        ExCommand::Clean { id, course, all } => {
            let scope = match (id, course, all) {
                (Some(id), _, _) => CleanScope::Exercise(id),
                (_, Some(course), _) => CleanScope::Course(course),
                (_, _, true) => CleanScope::All,
                _ => CleanScope::Here,
            };
            exercises::clean(scope, json)
        }
        ExCommand::Remove { id, force } => exercises::remove(&id, force, json),
    }
}

/// With `--json` the `latest_version` field says it instead.
fn notify_newer_version(session: &Session, json: bool) {
    if !json && let Some(notice) = session.newer_version_notice() {
        eprintln!("{notice}");
    }
}
