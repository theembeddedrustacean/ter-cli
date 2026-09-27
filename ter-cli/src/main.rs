use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod build;
mod config;
mod exercises;
mod hint;
mod hw;
mod install;
mod login;
mod output;
mod record;
mod run;
mod self_update;
mod serve;
mod session;
mod sim;
mod status;
mod telemetry;
mod token_store;
mod venues;
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
    /// Build the exercise, run it, check it and post the run to TER Learn.
    Run {
        /// The exercise folder; the one you are in by default.
        dir: Option<PathBuf>,
        /// Run in simulation this time, whatever ter.toml's `mode` says.
        #[arg(long, conflicts_with = "hw")]
        sim: bool,
        /// Run on hardware this time, whatever ter.toml's `mode` says.
        #[arg(long)]
        hw: bool,
        /// Where to run within the mode: `local` (your board on USB) on
        /// hardware, `wokwi` in simulation.
        #[arg(long)]
        venue: Option<String>,
        /// Build only: post the attempt without running or checking it.
        #[arg(long)]
        no_check: bool,
    },
    /// Your exercises and their last runs; --disk for disk use per course.
    Status {
        /// How much disk each course and exercise takes. Needs no account.
        #[arg(long)]
        disk: bool,
    },
    /// The next hint for the last run of the exercise you are in.
    Hint {
        /// The exercise folder; the one you are in by default.
        dir: Option<PathBuf>,
    },
    /// Where this machine can run exercises, and what each place can see.
    Venues,
    /// The local service the lesson page's Run button calls: it builds,
    /// runs, checks and posts from this machine.
    Serve {
        /// Listen on this port of 127.0.0.1 (default: `serve_port` in the
        /// config, 7357).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Recorded runs: judge one against a check.yaml, offline.
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// Install the tools ter and the exercises use. What is already
    /// installed is left alone.
    Install {
        /// `targets` (the Rust target of the project in this folder),
        /// `espflash`, `probe-rs`, `wokwi-cli` (with your Wokwi token).
        #[arg(required = true, value_parser = install::TOOLS)]
        tools: Vec<String>,
        /// The project `targets` reads, instead of the current folder.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Read the Wokwi token from standard input instead of asking.
        #[arg(long)]
        token_stdin: bool,
    },
    /// Update ter to the latest version.
    SelfUpdate,
}

#[derive(Subcommand)]
enum TelemetryCommand {
    /// Judge a recording (a `.runs/<n>` folder or its events.jsonl)
    /// against a check.yaml, and print the check.toml. Needs no account.
    Check {
        recording: PathBuf,
        /// The check.yaml to judge it against.
        #[arg(long)]
        check: PathBuf,
    },
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
        Command::Run {
            dir,
            sim,
            hw,
            venue,
            no_check,
        } => {
            let mode = match (sim, hw) {
                (true, _) => Some("simulation"),
                (_, true) => Some("hardware"),
                _ => None,
            };
            run::run(
                run::RunArgs {
                    dir,
                    mode,
                    venue,
                    no_check,
                },
                json,
            )
            .await
        }
        Command::Status { disk: true } => status::disk(json),
        Command::Status { disk: false } => status::status(json).await,
        Command::Hint { dir } => hint::hint(dir, json).await,
        Command::Venues => venues::run(json),
        Command::Serve { port } => serve::serve(port, json).await,
        Command::Telemetry {
            command: TelemetryCommand::Check { recording, check },
        } => telemetry::check(recording, check, json),
        Command::Install {
            tools,
            dir,
            token_stdin,
        } => install::run(&tools, dir, token_stdin, json).await,
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
