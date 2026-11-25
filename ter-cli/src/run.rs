//! `ter run`: build an exercise, run and judge it, and post the run.
//!
//! A checked run is a recording first: the venue writes `.runs/<n>/`, and
//! the verdicts come from judging that folder, exactly as `ter telemetry
//! check` would.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use ter_check::{CheckFile, CheckStatus, CheckVerdict, REPORT_FILE};
use ter_sdk::project::{Layout, TerToml, new_recording};
use ter_sdk::run::{LastRun, RunAnswer, RunRecord};
use ter_telemetry::local::Local;
use ter_telemetry::sim86::Sim86;
use ter_telemetry::wokwi::Wokwi;
use ter_telemetry::{Capture, Recording, Venue, rfc3339_utc};

use crate::build::{Abort, cargo_build};
use crate::config::Config;
use crate::output::{CliError, print_json};
use crate::record::{Infra, Outcome, RunContext, assemble, file_sha256};
use crate::session::Session;
use crate::telemetry::{RUN_CHECK, bad_check, check_recording};
use crate::{events, hw, remote, sim, sim86};
use ter_flash::Tool;

pub const CHECK_YAML: &str = "check.yaml";
pub const NO_CHECK_NOTE: &str = "This exercise has no automatic check yet. Run it with cargo run.";

pub struct RunArgs {
    pub dir: Option<PathBuf>,
    /// `--sim` or `--hw`: the mode for this run instead of ter.toml's.
    pub mode: Option<&'static str>,
    /// `--venue`: where to run within the mode, instead of ter.toml's.
    pub venue: Option<String>,
    /// Which shared bench, when the venue is `bench`, and how long to
    /// wait for it.
    pub bench: remote::Want,
    pub no_check: bool,
    /// Stops the build from elsewhere (`ter serve`, for a run the page
    /// dropped or replaced): the run then ends as `replaced`, before
    /// anything ran.
    pub abort: Option<Abort>,
}

/// A run that was posted, for printing or for `ter serve` to return.
pub struct Done {
    /// The run record as posted, the site's answer and the verdicts: what
    /// `ter run --json` prints.
    pub answer: Value,
    /// Why the run did not pass, if it did not. Already in `answer`.
    pub failure: Option<CliError>,
    record: RunRecord,
    site: RunAnswer,
    outcome: Outcome,
    built_in: Duration,
    ran_in: Option<(Duration, &'static str)>,
    local: bool,
    sim_allowed: bool,
}

/// What the run does after the build, settled before it.
enum Plan {
    /// `--no-check`.
    BuildOnly,
    /// No check.yaml: build only, whatever the mode.
    NoCheck,
    Wokwi(Box<Checked<sim::Ready>>),
    Sim86(Box<Checked<sim86::Ready>>),
    Local(Box<Checked<hw::Ready>>),
    /// Someone's shared bench; settled once the account is known.
    Bench(Box<Checked<Tool>>),
}

/// A checked run: the check.yaml, and the venue ready to run it.
struct Checked<R> {
    file: CheckFile,
    text: String,
    ready: R,
}

/// The simulators `ter run --sim` can name.
pub const SIM_VENUES: [&str; 2] = ["wokwi", "sim86"];

/// The default venue of each mode for the exercise in `dir`, and the ones
/// this ter can run.
pub fn venue_for(mode: &str, asked: Option<&str>, dir: &Path) -> Result<&'static str, CliError> {
    let (default, known): (&str, &[&'static str]) = match mode {
        "simulation" => (sim::default_venue(dir), &SIM_VENUES),
        _ => ("local", &["local", "bench"]),
    };
    let wanted = asked.unwrap_or(default);
    known.iter().copied().find(|v| *v == wanted).ok_or_else(|| {
        CliError::new(
            "venue_unavailable",
            format!(
                "ter cannot run in {mode} mode on venue {wanted:?}; it knows {}.",
                known.join(", ")
            ),
        )
    })
}

pub async fn run(args: RunArgs, json: bool) -> Result<(), CliError> {
    report(execute(args, json).await?, json)
}

/// A live run is on sim86 for now: refuse any other before the build.
pub fn only_sim86(args: &RunArgs) -> Result<(), CliError> {
    let dir = exercise_dir(args.dir.clone())?;
    let ter = TerToml::load(&dir)?;
    let mode = args.mode.unwrap_or(&ter.mode);
    let venue = venue_for(mode, args.venue.as_deref().or(ter.venue.as_deref()), &dir)?;
    if mode == "simulation" && venue == "sim86" {
        return Ok(());
    }
    Err(CliError::new(
        "bad_request",
        format!(
            "A live run is a simulation on sim86 for now, and this run would go to {venue} ({mode}). In a terminal: `ter run --sim sim86 --live`."
        ),
    ))
}

/// Print a posted run, and fail as `ter run` does when it did not pass.
pub fn report(done: Done, json: bool) -> Result<(), CliError> {
    if json {
        print_json(&done.answer);
    } else {
        print_text(&done);
    }
    match done.failure {
        Some(mut err) => {
            err.in_json_answer = json;
            Err(err)
        }
        None => Ok(()),
    }
}

/// Build, run, judge and post; `quiet` prints nothing but errors.
pub async fn execute(args: RunArgs, quiet: bool) -> Result<Done, CliError> {
    let mut built = build(args, quiet).await?;
    let outcome = built.on_venue()?;
    built.finish(outcome).await
}

/// A run whose build is over, failed or not, before anything ran: all
/// that running it on its venue and posting it need.
pub struct Built {
    dir: PathBuf,
    ter: TerToml,
    mode: String,
    venue: &'static str,
    sim_allowed: bool,
    plan: Plan,
    session: Session,
    user: String,
    bench: Option<remote::BenchReady>,
    number: u32,
    recording: PathBuf,
    started: Instant,
    build: crate::build::Build,
    log_path: PathBuf,
    previous: Option<LastRun>,
    elf_sha256: Option<String>,
    quiet: bool,
    ran_in: Option<(Duration, &'static str)>,
}

/// Everything up to and including the build: the plan for the venue, the
/// account, the run folder, cargo.
pub async fn build(args: RunArgs, quiet: bool) -> Result<Built, CliError> {
    let json = quiet;
    let dir = exercise_dir(args.dir)?;
    let ter = TerToml::load(&dir)?;
    let mode = args.mode.unwrap_or(&ter.mode).to_string();
    if !ter.modes.contains(&mode) {
        return Err(CliError::new(
            "mode_not_allowed",
            format!(
                "{} cannot be run in {mode} mode; it allows {}.",
                ter.exercise,
                ter.modes.join(" and ")
            ),
        ));
    }
    let venue = venue_for(&mode, args.venue.as_deref().or(ter.venue.as_deref()), &dir)?;
    // What ter points at as the run that completes the exercise.
    let sim_allowed = sim::offered(&dir, &ter.modes);

    let check_path = dir.join(CHECK_YAML);
    let plan = if args.no_check {
        Plan::BuildOnly
    } else if !check_path.is_file() {
        Plan::NoCheck
    } else {
        let text = std::fs::read_to_string(&check_path).map_err(|e| {
            CliError::new(
                "io_error",
                format!("Could not read {}: {e}", check_path.display()),
            )
        })?;
        let file = CheckFile::parse(&text).map_err(|e| bad_check(&check_path, e))?;
        if venue == "bench" {
            let ready = hw::tool(&dir)?;
            Plan::Bench(Box::new(Checked { file, text, ready }))
        } else if venue == "local" {
            let ready = hw::ready(&dir, !json)?;
            Plan::Local(Box::new(Checked { file, text, ready }))
        } else if venue == "sim86" {
            let ready = sim86::ready(&dir, sim86::plan(&dir, &ter, &file)?)?;
            Plan::Sim86(Box::new(Checked { file, text, ready }))
        } else {
            let ready = sim::ready(sim::plan(&dir, &ter, &file)?)?;
            Plan::Wokwi(Box::new(Checked { file, text, ready }))
        }
    };

    // Everything that would stop the post is checked before the build: a
    // token, a version the site accepts.
    let session = Session::open()?;
    let client = &session.client;
    client
        .ensure_supported()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?;
    let user = client.ping().await.map_err(CliError::from)?.user.clone();
    let bench = match &plan {
        Plan::Bench(b) => {
            let bench = remote::ready(&session, &user, &ter, &b.ready, &args.bench).await?;
            // Someone else's board is not spent on a run it cannot judge.
            let unseen = sim::unobservable(&b.file, &bench.info.provides);
            if !b.file.checks.is_empty() && unseen.len() == b.file.checks.len() {
                let ids: Vec<String> = unseen.into_iter().map(|(id, _)| id).collect();
                return Err(CliError::new(
                    "not_observable",
                    format!(
                        "{}'s bench {} cannot see any of this exercise's checks: {}.",
                        bench.link.owner_name,
                        bench.link.label,
                        hw::graded_only(&ids, sim_allowed)
                    ),
                ));
            }
            Some(bench)
        }
        _ => None,
    };

    let (number, recording) = new_recording(&dir)?;
    if !json {
        let how = match &plan {
            Plan::BuildOnly => "build only".to_string(),
            Plan::NoCheck => "build only, no check.yaml".to_string(),
            Plan::Wokwi(_) => "then Wokwi".to_string(),
            Plan::Sim86(_) => "then sim86".to_string(),
            Plan::Local(hw) => match &hw.ready.drive {
                Some(d) => format!("then the board through its UF2 drive {}", d.path.display()),
                None => format!("then the board on {}", hw.ready.port.path),
            },
            Plan::Bench(_) => {
                let b = bench.as_ref().expect("a bench plan has a bench");
                format!("then {}'s bench {}", b.link.owner_name, b.link.label)
            }
        };
        eprintln!("Building {} ({mode}, {how})", ter.exercise);
        if let Plan::Local(hw) = &plan
            && let Some(note) =
                hw::unseen_note(&hw.file, &hw.ready.provides(), "your board", sim_allowed)
        {
            eprintln!("{note}");
        }
        if let Plan::Local(hw) = &plan
            && hw.ready.drive.is_some()
        {
            eprintln!("ter hears this board through the USB serial port its program sets up.");
        }
        if let (Plan::Bench(b), Some(bench)) = (&plan, &bench)
            && let Some(note) =
                hw::unseen_note(&b.file, &bench.info.provides, "the bench", sim_allowed)
        {
            eprintln!("{note}");
        }
    }
    let started = Instant::now();
    let layout = Layout::new(Config::load()?.courses_root);
    let abort = args.abort;
    let stopped = |abort: &Option<Abort>| abort.as_ref().is_some_and(Abort::aborted);
    if stopped(&abort) {
        return Err(replaced());
    }
    let build = cargo_build(&dir, layout.cargo_env(&ter, &dir), !json, abort.as_ref())?;
    let log_path = recording.join("build.log");
    write(&log_path, build.log.as_bytes())?;
    if stopped(&abort) {
        return Err(replaced());
    }

    let previous =
        LastRun::load(&dir)?.filter(|last| last.user == user && last.site == client.site_url());
    let elf_sha256 = build.elf.as_deref().map(file_sha256).transpose()?;
    Ok(Built {
        dir,
        ter,
        mode,
        venue,
        sim_allowed,
        plan,
        session,
        user,
        bench,
        number,
        recording,
        started,
        build,
        log_path,
        previous,
        elf_sha256,
        quiet,
        ran_in: None,
    })
}

/// A run whose build was stopped: nothing ran, nothing is posted.
pub fn replaced() -> CliError {
    CliError::new(
        "replaced",
        "The run was stopped before it ran: the page dropped it or asked for a newer run.",
    )
}

/// A sim86 run that can be watched as it happens: the check, its text,
/// the venue ready to run it, the program and the run folder.
pub struct LiveParts<'a> {
    pub file: &'a CheckFile,
    pub text: &'a str,
    pub venue: Sim86,
    pub elf: &'a Path,
    pub recording: &'a Path,
    pub ter: &'a TerToml,
}

impl Built {
    /// What a live run needs, when this one is a checked run on sim86
    /// whose build gave a program. Any other run goes [`Self::on_venue`].
    pub fn live_parts(&self) -> Option<LiveParts<'_>> {
        let (Plan::Sim86(sim), true, Some(elf)) = (&self.plan, self.build.ok, &self.build.elf)
        else {
            return None;
        };
        let Checked { file, text, ready } = sim.as_ref();
        Some(LiveParts {
            file,
            text,
            venue: Sim86 {
                helper: ready.helper.clone(),
                board: ready.board.clone(),
                run_dir: self.recording.clone(),
                plan: ready.plan.clone(),
                image: ready.image.clone(),
                timeout_ms: file.timeout_ms,
                capture: capture(
                    &self.ter,
                    self.session.client.cli_version(),
                    &self.elf_sha256,
                ),
                program: None,
            },
            elf,
            recording: &self.recording,
            ter: &self.ter,
        })
    }

    /// The same sim86 run once more, for a live run started again: the
    /// same build and checks, and this run's folder until [`Self::afresh`]
    /// gives it its own.
    pub fn again(&self) -> Result<Built, CliError> {
        let Plan::Sim86(sim) = &self.plan else {
            return Err(CliError::new(
                "internal",
                "Only a run on sim86 is started again.",
            ));
        };
        Ok(Built {
            dir: self.dir.clone(),
            ter: self.ter.clone(),
            mode: self.mode.clone(),
            venue: self.venue,
            sim_allowed: self.sim_allowed,
            plan: Plan::Sim86(Box::new(Checked {
                file: sim.file.clone(),
                text: sim.text.clone(),
                ready: sim.ready.clone(),
            })),
            session: Session::open()?,
            user: self.user.clone(),
            bench: None,
            number: self.number,
            recording: self.recording.clone(),
            started: Instant::now(),
            build: crate::build::Build {
                ok: self.build.ok,
                log: self.build.log.clone(),
                elf: self.build.elf.clone(),
                duration: self.build.duration,
            },
            log_path: self.log_path.clone(),
            previous: self.previous.clone(),
            elf_sha256: self.elf_sha256.clone(),
            quiet: self.quiet,
            ran_in: None,
        })
    }

    /// A run folder of its own, with the build's log in it, and the
    /// exercise's last run as it is now.
    pub fn afresh(&mut self) -> Result<&Path, CliError> {
        let (number, recording) = new_recording(&self.dir)?;
        let log_path = recording.join("build.log");
        write(&log_path, self.build.log.as_bytes())?;
        let site = self.session.client.site_url().to_string();
        self.previous =
            LastRun::load(&self.dir)?.filter(|last| last.user == self.user && last.site == site);
        self.number = number;
        self.recording = recording;
        self.log_path = log_path;
        self.started = Instant::now();
        self.ran_in = None;
        Ok(&self.recording)
    }

    /// Say where the run ran and for how long, for the text `ter run`
    /// prints.
    pub fn ran(&mut self, took: Duration, on: &'static str) {
        self.ran_in = Some((took, on));
    }

    /// Run the program on the plan's venue and judge the recording; a
    /// failed build, or a run with nothing to check, is its own outcome.
    pub fn on_venue(&mut self) -> Result<Outcome, CliError> {
        let bench = self.bench.take();
        let json = self.quiet;
        let (plan, build, recording, ter, elf_sha256) = (
            &self.plan,
            &self.build,
            &self.recording,
            &self.ter,
            &self.elf_sha256,
        );
        let client = &self.session.client;
        let mut ran_in = None;
        let outcome = match (plan, &build.elf) {
            _ if !build.ok => Outcome::BuildFailed {
                log: build.log.clone(),
            },
            (Plan::BuildOnly, _) => Outcome::Built,
            (Plan::NoCheck, _) => Outcome::NoCheck,
            (Plan::Wokwi(_) | Plan::Sim86(_) | Plan::Local(_) | Plan::Bench(_), None) => {
                Outcome::Infra {
                    failure: Infra::VenueUnavailable,
                    output: "cargo built no program to run".into(),
                }
            }
            (Plan::Sim86(sim), Some(elf)) => {
                let Checked { file, text, ready } = sim.as_ref();
                if !json {
                    eprintln!(
                        "Running {} ms on sim86 (sending the program and starting the board takes a few seconds more)",
                        file.timeout_ms
                    );
                }
                let t = Instant::now();
                let mut venue = Sim86 {
                    helper: ready.helper.clone(),
                    board: ready.board.clone(),
                    run_dir: recording.to_path_buf(),
                    plan: ready.plan.clone(),
                    image: ready.image.clone(),
                    timeout_ms: file.timeout_ms,
                    capture: capture(ter, client.cli_version(), elf_sha256),
                    program: None,
                };
                let budget = Sim86::budget(file.timeout_ms);
                let outcome = record_and_judge(&mut venue, file, text, elf, budget, recording)?;
                ran_in = Some((t.elapsed(), "on sim86"));
                outcome
            }
            (Plan::Wokwi(sim), Some(elf)) => {
                let Checked { file, text, ready } = sim.as_ref();
                if !json {
                    eprintln!(
                        "Running {} ms in Wokwi (a few seconds more than that)",
                        file.timeout_ms
                    );
                }
                let t = Instant::now();
                let mut wokwi = Wokwi {
                    cli: ready.cli.clone(),
                    token: ready.token.clone(),
                    run_dir: recording.to_path_buf(),
                    plan: ready.plan.clone(),
                    timeout_ms: file.timeout_ms,
                    capture: capture(ter, client.cli_version(), elf_sha256),
                };
                let budget = wokwi_budget(file.timeout_ms);
                let outcome = record_and_judge(&mut wokwi, file, text, elf, budget, recording)?;
                ran_in = Some((t.elapsed(), "in Wokwi"));
                outcome
            }
            (Plan::Local(hw), Some(elf)) => {
                let Checked { file, text, ready } = hw.as_ref();
                if !json {
                    match &ready.drive {
                        Some(d) => eprintln!(
                            "Copying the program to {}, then {} ms of serial",
                            d.path.display(),
                            file.timeout_ms
                        ),
                        None => eprintln!(
                            "Flashing with {} on {}, then {} ms of serial",
                            ready.tool.program(),
                            ready.port.path,
                            file.timeout_ms
                        ),
                    }
                }
                let t = Instant::now();
                let mut local = Local::new(
                    ready.tool.clone(),
                    ready.program.clone(),
                    ready.port.clone(),
                    recording.to_path_buf(),
                    file.timeout_ms,
                );
                local.capture = capture(ter, client.cli_version(), elf_sha256);
                local.drive = ready.drive.clone();
                let budget = board_budget(file.timeout_ms);
                let outcome = record_and_judge(&mut local, file, text, elf, budget, recording)?;
                ran_in = Some((t.elapsed(), "on the board"));
                outcome
            }
            (Plan::Bench(b), Some(elf)) => {
                let Checked { file, text, .. } = b.as_ref();
                let bench = bench.expect("a bench plan has a bench");
                if !json {
                    eprintln!(
                        "Sending the program to {} at {}, then {} ms of serial",
                        bench.link.label, bench.url, file.timeout_ms
                    );
                }
                let t = Instant::now();
                let mut venue = bench.venue(ter, file.timeout_ms, recording.to_path_buf());
                venue.capture = capture(ter, client.cli_version(), elf_sha256);
                let budget = board_budget(file.timeout_ms);
                let outcome = record_and_judge(&mut venue, file, text, elf, budget, recording)?;
                ran_in = Some((t.elapsed(), "on the bench"));
                outcome
            }
        };
        self.ran_in = ran_in;
        Ok(outcome)
    }

    /// Post the run with `outcome` and keep it as the exercise's last run.
    pub async fn finish(self, outcome: Outcome) -> Result<Done, CliError> {
        let Built {
            dir,
            ter,
            mode,
            venue,
            sim_allowed,
            plan,
            session,
            user,
            number,
            recording,
            started,
            build,
            log_path,
            previous,
            elf_sha256,
            ran_in,
            ..
        } = self;
        let client = &session.client;
        let record = assemble(
            RunContext {
                ter: &ter,
                mode: &mode,
                venue: matches!(
                    plan,
                    Plan::Wokwi(_) | Plan::Sim86(_) | Plan::Local(_) | Plan::Bench(_)
                )
                .then_some(venue),
                elf_sha256,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                hints_used: previous.as_ref().map_or(0, |p| p.hints.used),
                cli_version: client.cli_version(),
            },
            &outcome,
        );

        let relative = PathBuf::from(ter_sdk::project::RUNS_DIR).join(number.to_string());
        let answer = client.post_run(&record).await.map_err(|e| {
            let mut err = session.forget_dead_token(e.into());
            err.message.push_str(&format!(
                " The run was not recorded on the site; its build log is in {}.",
                log_path.display()
            ));
            err
        })?;
        LastRun {
            user,
            site: client.site_url().to_string(),
            record: record.clone(),
            answer: answer.clone(),
            posted_at: rfc3339_utc(SystemTime::now()),
            recording: relative,
            hints: Default::default(),
            last_hint: None,
        }
        .save(&dir)?;

        let failure = failure(&outcome, &log_path, &recording);
        // What the venue recorded, for whoever shows the run. Read back from
        // the run folder, like the verdicts.
        let shown = matches!(outcome, Outcome::Checked { .. })
            .then(|| Recording::read(&recording).ok())
            .flatten()
            .map(|rec| {
                let nets = match &plan {
                    Plan::Sim86(sim) => sim
                        .ready
                        .plan
                        .watch
                        .iter()
                        .map(|w| (w.pin.clone(), w.net.clone()))
                        .collect(),
                    _ => Default::default(),
                };
                events::shown(&rec, &ter.pins, &nets)
            });
        let mut answer_json = answer_json(&record, &answer, &outcome, &recording, failure.as_ref());
        if let Some(shown) = shown {
            shown.put_into(&mut answer_json);
        }
        Ok(Done {
            answer: answer_json,
            failure,
            record,
            site: answer,
            outcome,
            built_in: build.duration,
            ran_in,
            local: matches!(plan, Plan::Local(_) | Plan::Bench(_)),
            sim_allowed,
        })
    }
}

/// How long Wokwi may take for `timeout_ms` of simulated time: wokwi-cli
/// paces the simulation step by step over the network, so far longer than
/// the simulated time.
pub fn wokwi_budget(timeout_ms: u64) -> Duration {
    Duration::from_secs(120) + Duration::from_millis(timeout_ms * 20)
}

/// How long a simulation of the exercise in `dir` may take, on the
/// simulator a run there uses when none is named.
pub fn sim_budget(dir: &Path, timeout_ms: u64) -> Duration {
    match sim::default_venue(dir) {
        "sim86" => Sim86::budget(timeout_ms),
        _ => wokwi_budget(timeout_ms),
    }
}

/// How long a capture on the board may take, once it is flashed.
pub fn board_budget(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms) + Duration::from_secs(5)
}

/// What every capture header carries, whatever the venue.
fn capture(ter: &TerToml, cli_version: &str, elf_sha256: &Option<String>) -> Capture {
    Capture {
        target: ter.target.clone(),
        cli_version: cli_version.to_string(),
        elf_sha256: elf_sha256.clone().unwrap_or_default(),
        ..Capture::default()
    }
}

/// Run the program on `venue`, record the run into `recording`, and judge
/// the recording.
fn record_and_judge(
    venue: &mut dyn Venue,
    file: &CheckFile,
    text: &str,
    elf: &Path,
    budget: Duration,
    recording: &Path,
) -> Result<Outcome, CliError> {
    let recorded = venue
        .prepare(elf)
        .and_then(|()| venue.reset())
        .and_then(|()| venue.run(&file.stimuli(), budget));
    match recorded {
        Ok(rec) => judge(&rec, text, recording),
        Err(e) => Ok(infra(&e)),
    }
}

/// A venue's failure as a run's outcome: posted as not run.
pub fn infra(e: &ter_telemetry::VenueError) -> Outcome {
    Outcome::Infra {
        failure: e.failure.into(),
        output: format!("{}\n{}", e.message, e.output),
    }
}

/// Write `rec` into the run folder `recording` and judge it there.
pub fn judge(rec: &Recording, text: &str, recording: &Path) -> Result<Outcome, CliError> {
    let events = recording.join(ter_telemetry::recording::EVENTS_FILE);
    rec.write(&events).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not write {}: {e}", events.display()),
        )
    })?;
    write(&recording.join(RUN_CHECK), text.as_bytes())?;

    // Judge what was written, not what is in memory.
    let (report, rec) = check_recording(recording, text, &recording.join(RUN_CHECK))?;
    write(&recording.join(REPORT_FILE), report.to_toml().as_bytes())?;
    Ok(Outcome::Checked {
        verdicts: report.verdicts,
        transcript: rec.serial_text(),
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    std::fs::write(path, bytes).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not write {}: {e}", path.display()),
        )
    })
}

/// `text` as a sentence: a venue's message ends without a full stop, or
/// with whatever a tool said last.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// The error `ter run` exits with after posting, if the run did not pass.
fn failure(outcome: &Outcome, log_path: &Path, recording: &Path) -> Option<CliError> {
    match outcome {
        Outcome::BuildFailed { .. } => Some(CliError::new(
            "build_failed",
            format!(
                "The build failed. The errors are above and in {}.",
                log_path.display()
            ),
        )),
        Outcome::Infra { failure, output } => Some(CliError::new(
            failure.code(),
            format!(
                "{} The run was posted as not run; it does not count against you. Details in {}.",
                sentence(output.lines().next().unwrap_or("The venue failed")),
                recording.display()
            ),
        )),
        Outcome::Checked { verdicts, .. } => verdicts
            .iter()
            .find(|v| v.status == CheckStatus::Fail)
            .map(|v| {
                CliError::new(
                    "check_failed",
                    format!(
                        "Check {} failed: expected {}, observed {}.",
                        v.id, v.expected, v.observed
                    ),
                )
            }),
        Outcome::Built | Outcome::NoCheck => None,
    }
}

/// The run record as posted, plus the site's answer, as `ter serve` will
/// return it to the lesson page. A judged run's answer also gets its
/// `events` (see [`events`]); those are not posted.
fn answer_json(
    record: &RunRecord,
    answer: &RunAnswer,
    outcome: &Outcome,
    recording: &Path,
    failure: Option<&CliError>,
) -> Value {
    let mut out = serde_json::to_value(record).expect("record serialises");
    out["name"] = json!(answer.name);
    out["site"] = json!({"attempt": answer.attempt, "concept_deltas": answer.concept_deltas});
    if let Some(completed) = answer.lesson_completed {
        out["site"]["lesson_completed"] = json!(completed);
    }
    out["recording"] = json!(recording);
    if let Outcome::Checked { verdicts, .. } = outcome {
        out["verdicts"] = json!(verdicts);
    }
    if let Some(err) = failure {
        out["error"] = json!({"code": err.code, "message": err.message});
    }
    out
}

/// `point_at_sim`: end the partial line with what completes the exercise
/// (not when the board-pass line will say it).
fn print_verdicts(verdicts: &[CheckVerdict], sim_allowed: bool, point_at_sim: bool) {
    let width = verdicts.iter().map(|v| v.id.len()).max().unwrap_or(0);
    for v in verdicts {
        let status = match v.status {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "FAIL",
            CheckStatus::NotObservable => "not graded",
        };
        println!(
            "  {status:<10}  {:<width$}  expected {}; observed {}",
            v.id, v.expected, v.observed
        );
    }
    if let Some(line) = partial_line(verdicts, sim_allowed, point_at_sim) {
        println!("{line}");
    }
}

/// The line that says a run could not grade every check, so it cannot
/// complete the exercise, if it could not.
fn partial_line(
    verdicts: &[CheckVerdict],
    sim_allowed: bool,
    point_at_sim: bool,
) -> Option<String> {
    let unseen: Vec<String> = verdicts
        .iter()
        .filter(|v| v.status == CheckStatus::NotObservable)
        .map(|v| v.id.clone())
        .collect();
    let seen = verdicts.len() - unseen.len();
    let all_passed = verdicts.iter().all(|v| v.status != CheckStatus::Fail);
    if unseen.is_empty() || !all_passed {
        return None;
    }
    let why = hw::graded_only(&unseen, sim_allowed);
    let mut line = if seen == 0 {
        format!("No check can be graded here, so the run is posted as not run: {why}.")
    } else {
        format!(
            "{seen} of {} checks passed here; {why}, so this run can't complete the exercise.",
            verdicts.len()
        )
    };
    if sim_allowed && point_at_sim {
        line.push_str(" To complete it: ter run --sim");
    }
    Some(line)
}

/// The line that says a pass on a board did not complete the lesson, when
/// the site says so: the simulation run completes it, or, for a
/// hardware-only exercise, the lesson page does.
fn board_pass_line(record: &RunRecord, answer: &RunAnswer, sim_allowed: bool) -> Option<String> {
    let on = match record.venue.as_deref() {
        Some("local") => "your board",
        Some("bench") => "the bench",
        _ => return None,
    };
    if record.check_status != "passed" || answer.lesson_completed != Some(false) {
        None
    } else if sim_allowed {
        Some(format!(
            "Passed on {on}. Run `ter run --sim` to complete the lesson."
        ))
    } else {
        Some(format!(
            "Passed on {on}, but some checks are graded only with a telemetry board. \
             Mark the lesson complete on the lesson page to continue."
        ))
    }
}

/// The last `n` lines of what the board printed.
fn serial_tail(transcript: &str, n: usize) -> String {
    let lines: Vec<&str> = transcript.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn print_text(done: &Done) {
    let Done {
        record,
        site: answer,
        outcome,
        built_in,
        ran_in,
        local,
        sim_allowed,
        ..
    } = done;
    let (local, sim_allowed) = (*local, *sim_allowed);
    let board_pass = board_pass_line(record, answer, sim_allowed);
    if record.build_status == "passed" {
        print!("Built in {:.1} s.", built_in.as_secs_f64());
        if let Some((t, venue)) = *ran_in {
            print!(" Ran {venue} in {:.1} s.", t.as_secs_f64());
        }
        println!();
    } else {
        println!("Build failed.");
    }
    match outcome {
        Outcome::Built => println!("Not checked: --no-check builds only."),
        Outcome::NoCheck => println!("{NO_CHECK_NOTE}"),
        Outcome::Checked {
            verdicts,
            transcript,
        } => {
            if local && !transcript.trim().is_empty() {
                println!("The board printed:");
                for line in serial_tail(transcript, 20).lines() {
                    println!("  | {line}");
                }
            }
            // The board-pass line points at --sim instead.
            print_verdicts(verdicts, sim_allowed, board_pass.is_none())
        }
        Outcome::Infra { .. } | Outcome::BuildFailed { .. } => {}
    }
    println!("Posted attempt {} to TER Learn.", answer.attempt);
    for d in &answer.concept_deltas {
        println!(
            "  {:<24} {:.2} -> {:.2}",
            d.label.as_deref().unwrap_or(&d.concept),
            d.before,
            d.after
        );
    }
    if let Some(line) = board_pass {
        println!("{line}");
    }
    if record.build_status == "failed" || record.check_status == "failed" {
        println!("Stuck? `ter hint` gives a hint for this run.");
    }
}

/// `dir`, or the exercise folder the current directory is in.
pub fn exercise_dir(dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    let start = match dir {
        Some(d) => d,
        None => std::env::current_dir()
            .map_err(|e| CliError::new("io_error", format!("No current directory: {e}")))?,
    };
    ter_sdk::project::enclosing_exercise(&start).ok_or_else(|| {
        CliError::new(
            "not_an_exercise",
            format!(
                "{} is not in an exercise folder. Fetch one with `ter ex fetch <id>` and run this in it.",
                start.display()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_venue_said_ends_before_the_run_is_called_not_run() {
        let said = |output: &str| {
            failure(
                &Outcome::Infra {
                    failure: Infra::VenueUnavailable,
                    output: output.into(),
                },
                Path::new("build.log"),
                Path::new("runs/3"),
            )
            .unwrap()
            .message
        };
        assert_eq!(
            said("this program may not set one up\nthe tool's output"),
            "this program may not set one up. The run was posted as not run; \
             it does not count against you. Details in runs/3."
        );
        assert!(
            said("Is it a RP2040's drive?\n").starts_with("Is it a RP2040's drive? The run"),
            "a question keeps its mark"
        );
        assert!(said("").starts_with("The venue failed. The run"));
    }

    #[test]
    fn a_pass_on_a_board_that_did_not_complete_the_lesson_points_at_sim() {
        let record = |venue: &str, check: &str| RunRecord {
            venue: Some(venue.into()),
            check_status: check.into(),
            ..RunRecord::default()
        };
        let answer = |completed| RunAnswer {
            name: "r1".into(),
            attempt: 1,
            concept_deltas: vec![],
            lesson_completed: completed,
        };
        let board = record("local", "passed");
        assert_eq!(
            board_pass_line(&board, &answer(Some(false)), true).unwrap(),
            "Passed on your board. Run `ter run --sim` to complete the lesson."
        );
        assert_eq!(
            board_pass_line(&record("bench", "passed"), &answer(Some(false)), true).unwrap(),
            "Passed on the bench. Run `ter run --sim` to complete the lesson."
        );
        assert_eq!(board_pass_line(&board, &answer(Some(true)), true), None);
        assert_eq!(board_pass_line(&board, &answer(None), true), None);
        assert_eq!(
            board_pass_line(&board, &answer(Some(false)), false).unwrap(),
            "Passed on your board, but some checks are graded only with a telemetry board. \
             Mark the lesson complete on the lesson page to continue."
        );
        assert_eq!(board_pass_line(&board, &answer(Some(true)), false), None);
        let failed = record("local", "failed");
        assert_eq!(board_pass_line(&failed, &answer(Some(false)), true), None);
        let sim = record("wokwi", "passed");
        assert_eq!(board_pass_line(&sim, &answer(Some(false)), true), None);
    }

    #[test]
    fn a_partial_pass_says_it_cannot_complete_the_exercise() {
        let v = |id: &str, status| CheckVerdict {
            id: id.into(),
            status,
            expected: String::new(),
            observed: String::new(),
        };
        use CheckStatus::{Fail, NotObservable, Pass};
        let some = [
            v("banner", Pass),
            v("blink", NotObservable),
            v("rate", NotObservable),
        ];
        assert_eq!(
            partial_line(&some, true, true).unwrap(),
            "1 of 3 checks passed here; blink, rate are graded only in simulation, \
             so this run can't complete the exercise. To complete it: ter run --sim"
        );
        assert_eq!(
            partial_line(&some, true, false).unwrap(),
            "1 of 3 checks passed here; blink, rate are graded only in simulation, \
             so this run can't complete the exercise."
        );
        assert_eq!(
            partial_line(&some, false, true).unwrap(),
            "1 of 3 checks passed here; blink, rate are graded only with a telemetry board, \
             which ter does not support yet, so this run can't complete the exercise."
        );
        let none = [v("blink", NotObservable)];
        assert_eq!(
            partial_line(&none, true, true).unwrap(),
            "No check can be graded here, so the run is posted as not run: \
             blink is graded only in simulation. To complete it: ter run --sim"
        );
        assert_eq!(partial_line(&[v("a", Pass)], true, true), None);
        assert_eq!(
            partial_line(&[v("a", Fail), v("b", NotObservable)], true, true),
            None
        );
    }

    #[test]
    fn each_mode_has_its_venue() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        assert_eq!(venue_for("simulation", None, dir).unwrap(), "wokwi");
        assert_eq!(venue_for("hardware", None, dir).unwrap(), "local");
        assert_eq!(
            venue_for("simulation", Some("wokwi"), dir).unwrap(),
            "wokwi"
        );
        assert_eq!(
            venue_for("simulation", Some("sim86"), dir).unwrap(),
            "sim86"
        );
        let err = venue_for("simulation", Some("renode"), dir).unwrap_err();
        assert_eq!(err.code, "venue_unavailable");
        assert!(err.message.contains("wokwi, sim86"), "{}", err.message);
        assert!(venue_for("hardware", Some("wokwi"), dir).is_err());
        assert!(venue_for("hardware", Some("sim86"), dir).is_err());

        // A sim86 board and no Wokwi circuit: sim86 is what `--sim` runs.
        std::fs::create_dir_all(dir.join("sim86")).unwrap();
        std::fs::write(dir.join("sim86/board.json"), "{}").unwrap();
        std::fs::write(dir.join("sim86/pins.json"), "{}").unwrap();
        assert_eq!(venue_for("simulation", None, dir).unwrap(), "sim86");
        assert_eq!(sim_budget(dir, 1000), Sim86::budget(1000));
        // With a Wokwi circuit too, Wokwi stays the default.
        std::fs::write(dir.join("diagram.json"), "{}").unwrap();
        assert_eq!(venue_for("simulation", None, dir).unwrap(), "wokwi");
        assert_eq!(sim_budget(dir, 1000), wokwi_budget(1000));
    }
}
