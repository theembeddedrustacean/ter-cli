use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod bench;
mod build;
mod config;
mod doctor;
mod events;
mod exercises;
mod hint;
mod hw;
mod install;
mod live;
mod llm;
mod login;
mod new;
mod output;
mod record;
mod remote;
mod run;
mod self_update;
mod serve;
mod session;
mod share;
mod sim;
mod sim86;
mod status;
mod survey;
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

    /// Keep courses in this folder for this command, instead of the one in
    /// TER_COURSES_ROOT or `courses_root` in ter's config.toml (default:
    /// ~/ter-courses).
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

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
    /// Say what is missing or wrong on this machine, and how to fix it.
    ///
    /// Only looks: nothing is installed, built or flashed. In an exercise
    /// or project folder it judges for that folder's run.
    Doctor {
        /// The folder to judge for; the one you are in by default.
        dir: Option<PathBuf>,
        /// Do not ask TER Learn anything.
        #[arg(long)]
        offline: bool,
        /// Also compare the tools with the versions a curriculum checkout
        /// is checked with.
        #[arg(long, value_name = "CURRICULUM_PATH")]
        dev: Option<PathBuf>,
    },
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
        /// `--sim sim86` runs it on sim86 instead of Wokwi.
        #[arg(
            long,
            conflicts_with = "hw",
            num_args = 0..=1,
            default_missing_value = "",
            value_name = "SIMULATOR"
        )]
        sim: Option<String>,
        /// Run on hardware this time, whatever ter.toml's `mode` says.
        #[arg(long)]
        hw: bool,
        /// Where to run within the mode: `local` (your board on USB) or
        /// `bench` on hardware, `wokwi` or `sim86` in simulation.
        #[arg(long)]
        venue: Option<String>,
        /// Build only: post the attempt without running or checking it.
        #[arg(long)]
        no_check: bool,
        /// Watch the run as it happens (sim86 only): prints serial and
        /// pin changes until Ctrl-C or the limit, then the verdicts.
        #[arg(long, conflicts_with_all = ["hw", "no_check"])]
        live: bool,
        /// How long a `--live` run may last, in seconds (600 at most).
        /// sim86 charges your account for every minute it runs.
        #[arg(long, requires = "live", value_name = "SECONDS")]
        limit: Option<u64>,
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
        /// Ask your own model instead (`ter llm setup` first), on your
        /// account with its provider.
        #[arg(long)]
        llm: bool,
        /// With --llm: print what would be sent to the model, send nothing.
        #[arg(long, requires = "llm")]
        dry_run: bool,
    },
    /// The model `ter hint --llm` asks, with your own key.
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
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
        /// Also answer a lesson page served from http://localhost:8080 (a
        /// development copy of the site). Without it only the lesson page
        /// of the site this ter is set to (TER Learn, unless the config
        /// says otherwise) can start a run.
        #[arg(long)]
        dev: bool,
        /// Also share the board on this machine as a bench: register it on
        /// TER Learn, share it under a code, and take runs from people who
        /// connect with that code. They flash their own firmware onto your
        /// board, which can burn eFuses or short pins: share only with
        /// people you trust.
        #[arg(long, requires = "board")]
        share: bool,
        /// The board you share, as a target id. Only `xiao-esp32c3` can be
        /// shared for now.
        #[arg(long, requires = "share")]
        board: Option<String>,
        /// A name for the bench on the Devices page (default: the board).
        #[arg(long, requires = "share")]
        label: Option<String>,
        /// Where others reach the bench: your tunnel's or LAN address
        /// (default: the address it listens on).
        #[arg(long, requires = "share")]
        url: Option<String>,
        /// The address the bench listens on (default 127.0.0.1, for a
        /// tunnel; 0.0.0.0 for the LAN, with --url).
        #[arg(long, requires = "share", default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        /// The bench's port (default: the page's port plus one).
        #[arg(long, requires = "share")]
        bench_port: Option<u16>,
    },
    /// Connect to a bench someone shares with you, by its code.
    Connect {
        /// The share code (`WXYZ-1234`).
        code: String,
    },
    /// Benches: yours, and the ones you connected to.
    Bench {
        #[command(subcommand)]
        command: BenchCommand,
    },
    /// Recorded runs: judge one against a check.yaml, offline.
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// A new project for a board: the XIAO boards are generated by
    /// xiao-generate (everything after `--` goes to it unchanged), the
    /// Nucleo-F401RE by ter's own template.
    New {
        /// The board, as `ter new --list-boards` names it (`xiao-esp32c3`,
        /// `nucleo-f401re`).
        #[arg(long, required_unless_present_any = ["list_boards", "list_hals", "list_templates"])]
        board: Option<String>,
        /// The HAL (`esp-hal`, `esp-idf`, `embassy-rp`, `stm32f4xx-hal`, ...); the board's
        /// default when left out. `ter new --list-hals --board B` lists them.
        #[arg(long)]
        hal: Option<String>,
        /// Async code on the embassy framework instead of blocking code.
        #[arg(long = "async")]
        r#async: bool,
        /// The project's crate name.
        #[arg(long, required_unless_present_any = ["list_boards", "list_hals", "list_templates"])]
        name: Option<String>,
        /// Where to put the project (default: ./<name>).
        #[arg(long)]
        out: Option<PathBuf>,
        /// The template (`ter new --list-templates --board B`).
        #[arg(long)]
        template: Option<String>,
        /// Logging: `none`, `defmt`, or `log` on esp-hal.
        #[arg(long)]
        logging: Option<String>,
        /// The flashing tool, e.g. `espflash` or `probe-rs`.
        #[arg(long)]
        flashing: Option<String>,
        /// Let xiao-generate install what the project needs without asking.
        #[arg(long, conflicts_with = "no_install")]
        yes: bool,
        /// Never let xiao-generate install anything.
        #[arg(long)]
        no_install: bool,
        /// Accepted for scripts written for xiao-generate; ter new never
        /// asks questions.
        #[arg(long, hide = true)]
        headless: bool,
        /// List the boards: xiao-generate's, then ter's own.
        #[arg(long, conflicts_with_all = ["list_hals", "list_templates"])]
        list_boards: bool,
        /// List the HALs, for one board with --board.
        #[arg(long, conflicts_with = "list_templates")]
        list_hals: bool,
        /// List the templates, for one board with --board.
        #[arg(long)]
        list_templates: bool,
        /// More xiao-generate flags, after `--`.
        #[arg(last = true)]
        extra: Vec<std::ffi::OsString>,
    },
    /// Install the tools ter and the exercises use. What is already
    /// installed is left alone.
    Install {
        /// `targets` (the Rust target of the project in this folder),
        /// `xiao-generate` (for `ter new`), `espflash`, `probe-rs`,
        /// `elf2uf2-rs`, `uf2deploy` (UF2 boards' `cargo run`),
        /// `wokwi-cli` (with your Wokwi token), `sim86` (the sim86
        /// simulator, for `ter run --sim sim86`; needs Python).
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
enum LlmCommand {
    /// Choose the provider and model, and store your key on this machine.
    Setup {
        /// openai, anthropic, gemini, openrouter, ollama or
        /// openai-compatible (with --base-url).
        #[arg(long)]
        provider: String,
        /// The model (default: the provider's usual one, where ter has one).
        #[arg(long)]
        model: Option<String>,
        /// The provider's API address, instead of its usual one.
        #[arg(long)]
        base_url: Option<String>,
        /// Read the key from standard input instead of asking.
        #[arg(long)]
        key_stdin: bool,
        /// Share hints from the model with TER Learn.
        #[arg(long, conflicts_with = "no_share")]
        share: bool,
        /// Keep hints from the model on this machine.
        #[arg(long)]
        no_share: bool,
    },
    /// The provider, model and where the key is kept (never the key).
    Status,
    /// Remove your key from this machine.
    Forget {
        /// The provider's key to remove (default: the one set up).
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Subcommand)]
enum BenchCommand {
    /// Your benches and the ones you connected to.
    List,
    /// Share one of your benches; prints the code to hand out.
    Share {
        /// Its name or label; the only one when you have one.
        bench: Option<String>,
    },
    /// Stop sharing: the code stops working and everyone connected is
    /// dropped.
    Unshare { bench: Option<String> },
    /// Delete one of your benches and every share of it.
    Remove { bench: Option<String> },
    /// End your session on a bench you connected to (the newest when not
    /// named).
    Disconnect { bench: Option<String> },
    /// Take a bench you connected to off your list.
    Forget { bench: Option<String> },
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
    ///
    /// It goes to <courses-root>/<course>/<id>. The courses root is
    /// ~/ter-courses unless you set another, in this order: `--root DIR`
    /// for one command, the TER_COURSES_ROOT environment variable, or
    /// `courses_root = "DIR"` in ter's config.toml (~/.config/ter on
    /// Linux). To put one exercise somewhere else, name the folder:
    /// `ter ex fetch <id> <dir>`.
    ///
    /// The other commands look for fetched exercises under the same root,
    /// so set it once in the environment or the config rather than passing
    /// --root each time.
    Fetch {
        /// The exercise id, as `ter ex list` shows it.
        id: String,
        /// Put it here instead of <courses-root>/<course>/<id>.
        dir: Option<PathBuf>,
        /// Read it from a local curriculum checkout instead of the site.
        #[arg(long, value_name = "CURRICULUM_PATH")]
        dev: Option<PathBuf>,
    },
    /// What an exercise checks and which run can pass it: on your board
    /// (`ter run --hw`), in simulation (`ter run --sim`), or both.
    ///
    /// With no argument, the exercise in the current folder; or name a
    /// folder or an exercise id. A fetched exercise is read offline, from
    /// its check.yaml as fetched; an id not fetched yet is asked of the
    /// site once and nothing is written. It grades and runs nothing.
    Show {
        /// An exercise id or folder; the one you are in by default.
        #[arg(value_name = "ID_OR_DIR")]
        what: Option<String>,
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
    // An empty value, from the flag or the variable, names no folder.
    let named = |root: PathBuf| Some(root).filter(|r| !r.as_os_str().is_empty());
    let root = cli.root.clone().and_then(named).or_else(|| {
        std::env::var_os("TER_COURSES_ROOT")
            .map(PathBuf::from)
            .and_then(named)
    });
    if let Some(root) = root {
        config::set_courses_root(&root);
    }
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
        Command::Doctor { dir, offline, dev } => {
            doctor::run(doctor::Args { dir, offline, dev }, json).await
        }
        Command::Ex { command } => ex(command, json).await,
        Command::Run {
            dir,
            sim,
            hw,
            venue,
            no_check,
            live,
            limit,
        } => {
            let (dir, venue) = sim_names(sim.as_deref(), dir, venue)?;
            let mode = match (sim.is_some(), hw) {
                (true, _) => Some("simulation"),
                (_, true) => Some("hardware"),
                _ => None,
            };
            let args = run::RunArgs {
                dir,
                mode,
                venue,
                bench: Default::default(),
                no_check,
                abort: None,
            };
            if live {
                let limit = limit.unwrap_or(ter_remote::serve::LIVE_LIMIT_S);
                live::run(args, limit, json).await
            } else {
                run::run(args, json).await
            }
        }
        Command::Status { disk: true } => status::disk(json),
        Command::Status { disk: false } => status::status(json).await,
        Command::Hint {
            dir, llm: false, ..
        } => hint::hint(dir, json).await,
        Command::Hint { dir, dry_run, .. } => llm::hint(dir, dry_run, json).await,
        Command::Llm { command } => match command {
            LlmCommand::Setup {
                provider,
                model,
                base_url,
                key_stdin,
                share,
                no_share,
            } => llm::setup(
                llm::Setup {
                    provider,
                    model,
                    base_url,
                    key_stdin,
                    share: (share || no_share).then_some(share),
                },
                json,
            ),
            LlmCommand::Status => llm::status(json),
            LlmCommand::Forget { provider } => llm::forget(provider, json),
        },
        Command::Venues => venues::run(json),
        Command::Serve {
            port,
            dev,
            share,
            board,
            label,
            url,
            bind,
            bench_port,
        } => {
            let share = share.then(|| serve::ShareFlags {
                board: board.unwrap_or_default(),
                label,
                url,
                bind,
                bench_port,
            });
            serve::serve(port, dev, share, json).await
        }
        Command::Connect { code } => bench::connect(&code, json).await,
        Command::Bench { command } => match command {
            BenchCommand::List => bench::list(json).await,
            BenchCommand::Share { bench } => bench::share(bench, json).await,
            BenchCommand::Unshare { bench } => bench::unshare(bench, json).await,
            BenchCommand::Remove { bench } => bench::remove(bench, json).await,
            BenchCommand::Disconnect { bench } => bench::disconnect(bench, false, json).await,
            BenchCommand::Forget { bench } => bench::disconnect(bench, true, json).await,
        },
        Command::Telemetry {
            command: TelemetryCommand::Check { recording, check },
        } => telemetry::check(recording, check, json),
        Command::New {
            board,
            hal,
            r#async,
            name,
            out,
            template,
            logging,
            flashing,
            yes,
            no_install,
            headless: _,
            list_boards,
            list_hals,
            list_templates,
            extra,
        } => {
            use ter_gen::new::Listing;
            let listing = match (list_boards, list_hals, list_templates) {
                (true, _, _) => Some(Listing::Boards),
                (_, true, _) => Some(Listing::Hals),
                (_, _, true) => Some(Listing::Templates),
                _ => None,
            };
            if let Some(listing) = listing {
                return new::list(listing, board.as_deref(), json);
            }
            new::new(
                ter_gen::new::NewArgs {
                    board: board.unwrap_or_default(),
                    hal,
                    r#async,
                    name: name.unwrap_or_default(),
                    out,
                    template,
                    logging,
                    flashing,
                    yes,
                    no_install,
                    extra,
                },
                json,
            )
        }
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

/// What the word after `--sim` names: a simulator (`--sim sim86`), or,
/// as before simulators could be named, the exercise folder.
fn sim_names(
    sim: Option<&str>,
    dir: Option<PathBuf>,
    venue: Option<String>,
) -> Result<(Option<PathBuf>, Option<String>), CliError> {
    match sim {
        None | Some("") => Ok((dir, venue)),
        Some(named) if run::SIM_VENUES.contains(&named) => match venue {
            Some(other) if other != named => Err(CliError::new(
                "bad_request",
                format!("--sim {named} and --venue {other} name two venues; give one."),
            )),
            _ => Ok((dir, Some(named.to_string()))),
        },
        Some(folder) if dir.is_none() => Ok((Some(PathBuf::from(folder)), venue)),
        Some(other) => Err(CliError::new(
            "bad_request",
            format!(
                "--sim {other}: ter knows the simulators {}.",
                run::SIM_VENUES.join(" and ")
            ),
        )),
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
        ExCommand::Show { what } => exercises::show(what, json).await,
        ExCommand::Remove { id, force } => exercises::remove(&id, force, json),
    }
}

/// With `--json` the `latest_version` field says it instead.
fn notify_newer_version(session: &Session, json: bool) {
    if !json && let Some(notice) = session.newer_version_notice() {
        eprintln!("{notice}");
    }
}
