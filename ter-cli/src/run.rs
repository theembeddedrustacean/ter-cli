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
use ter_telemetry::wokwi::Wokwi;
use ter_telemetry::{Capture, Venue, rfc3339_utc};

use crate::build::cargo_build;
use crate::config::Config;
use crate::output::{CliError, print_json};
use crate::record::{Infra, Outcome, RunContext, assemble, file_sha256};
use crate::session::Session;
use crate::telemetry::{RUN_CHECK, bad_check, check_recording};
use crate::{hw, remote, sim};
use ter_flash::Tool;

pub const CHECK_YAML: &str = "check.yaml";
pub const NO_CHECK_NOTE: &str = "This exercise has no automatic check yet. Run it with cargo run.";

pub struct RunArgs {
    pub dir: Option<PathBuf>,
    /// `--sim` or `--hw`: the mode for this run instead of ter.toml's.
    pub mode: Option<&'static str>,
    /// `--venue`: where to run within the mode, instead of ter.toml's.
    pub venue: Option<String>,
    pub no_check: bool,
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

/// The default venue of each mode, and the ones this ter can run.
fn venue_for(mode: &str, asked: Option<&str>) -> Result<&'static str, CliError> {
    let (default, known): (&str, &[&'static str]) = match mode {
        "simulation" => ("wokwi", &["wokwi"]),
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
    let done = execute(args, json).await?;
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
    let venue = venue_for(&mode, args.venue.as_deref().or(ter.venue.as_deref()))?;

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
            let ready = hw::ready(&dir)?;
            Plan::Local(Box::new(Checked { file, text, ready }))
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
        Plan::Bench(b) => Some(remote::ready(&session, &user, &ter, &b.ready).await?),
        _ => None,
    };

    let (number, recording) = new_recording(&dir)?;
    if !json {
        let how = match &plan {
            Plan::BuildOnly => "build only".to_string(),
            Plan::NoCheck => "build only, no check.yaml".to_string(),
            Plan::Wokwi(_) => "then Wokwi".to_string(),
            Plan::Local(hw) => format!("then the board on {}", hw.ready.port.path),
            Plan::Bench(_) => {
                let b = bench.as_ref().expect("a bench plan has a bench");
                format!("then {}'s bench {}", b.link.owner_name, b.link.label)
            }
        };
        eprintln!("Building {} ({mode}, {how})", ter.exercise);
        if let Plan::Local(hw) = &plan
            && let Some(note) = hw::unseen_note(&hw.file, &hw.ready.provides())
        {
            eprintln!("{note}");
        }
        if let (Plan::Bench(b), Some(bench)) = (&plan, &bench)
            && let Some(note) = remote::unseen_note(&b.file, &bench.info.provides)
        {
            eprintln!("{note}");
        }
    }
    let started = Instant::now();
    let layout = Layout::new(Config::load()?.courses_root);
    let build = cargo_build(&dir, layout.cargo_env(&ter, &dir), !json)?;
    let log_path = recording.join("build.log");
    write(&log_path, build.log.as_bytes())?;

    let previous =
        LastRun::load(&dir)?.filter(|last| last.user == user && last.site == client.site_url());
    let elf_sha256 = build.elf.as_deref().map(file_sha256).transpose()?;
    let mut ran_in = None;
    let outcome = match (&plan, &build.elf) {
        _ if !build.ok => Outcome::BuildFailed {
            log: build.log.clone(),
        },
        (Plan::BuildOnly, _) => Outcome::Built,
        (Plan::NoCheck, _) => Outcome::NoCheck,
        (Plan::Wokwi(_) | Plan::Local(_) | Plan::Bench(_), None) => Outcome::Infra {
            failure: Infra::VenueUnavailable,
            output: "cargo built no program to run".into(),
        },
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
                capture: capture(&ter, client.cli_version(), &elf_sha256),
            };
            let budget = wokwi_budget(file.timeout_ms);
            let outcome = record_and_judge(&mut wokwi, file, text, elf, budget, &recording)?;
            ran_in = Some((t.elapsed(), "in Wokwi"));
            outcome
        }
        (Plan::Local(hw), Some(elf)) => {
            let Checked { file, text, ready } = hw.as_ref();
            if !json {
                eprintln!(
                    "Flashing with {} on {}, then {} ms of serial",
                    ready.tool.program(),
                    ready.port.path,
                    file.timeout_ms
                );
            }
            let t = Instant::now();
            let mut local = Local::new(
                ready.tool.clone(),
                ready.program.clone(),
                ready.port.clone(),
                recording.to_path_buf(),
                file.timeout_ms,
            );
            local.capture = capture(&ter, client.cli_version(), &elf_sha256);
            let budget = board_budget(file.timeout_ms);
            let outcome = record_and_judge(&mut local, file, text, elf, budget, &recording)?;
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
            let mut venue = bench.venue(&ter, file.timeout_ms, recording.to_path_buf());
            venue.capture = capture(&ter, client.cli_version(), &elf_sha256);
            let budget = board_budget(file.timeout_ms);
            let outcome = record_and_judge(&mut venue, file, text, elf, budget, &recording)?;
            ran_in = Some((t.elapsed(), "on the bench"));
            outcome
        }
    };
    let record = assemble(
        RunContext {
            ter: &ter,
            mode: &mode,
            venue: matches!(plan, Plan::Wokwi(_) | Plan::Local(_) | Plan::Bench(_))
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
    Ok(Done {
        answer: answer_json(&record, &answer, &outcome, &recording, failure.as_ref()),
        failure,
        record,
        site: answer,
        outcome,
        built_in: build.duration,
        ran_in,
        local: matches!(plan, Plan::Local(_) | Plan::Bench(_)),
        sim_allowed: ter.modes.iter().any(|m| m == "simulation"),
    })
}

/// How long Wokwi may take for `timeout_ms` of simulated time: wokwi-cli
/// paces the simulation step by step over the network, so far longer than
/// the simulated time.
pub fn wokwi_budget(timeout_ms: u64) -> Duration {
    Duration::from_secs(120) + Duration::from_millis(timeout_ms * 20)
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
    let rec = match recorded {
        Ok(rec) => rec,
        Err(e) => {
            return Ok(Outcome::Infra {
                failure: e.failure.into(),
                output: format!("{}\n{}", e.message, e.output),
            });
        }
    };
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
                output.lines().next().unwrap_or("The venue failed."),
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
/// return it to the lesson page.
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
    out["recording"] = json!(recording);
    if let Outcome::Checked { verdicts, .. } = outcome {
        out["verdicts"] = json!(verdicts);
    }
    if let Some(err) = failure {
        out["error"] = json!({"code": err.code, "message": err.message});
    }
    out
}

fn print_verdicts(verdicts: &[CheckVerdict], sim_allowed: bool) {
    let width = verdicts.iter().map(|v| v.id.len()).max().unwrap_or(0);
    for v in verdicts {
        let status = match v.status {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "FAIL",
            CheckStatus::NotObservable => "unseen",
        };
        println!(
            "  {status:<6}  {:<width$}  expected {}; observed {}",
            v.id, v.expected, v.observed
        );
    }
    if let Some(line) = partial_line(verdicts, sim_allowed) {
        println!("{line}");
    }
}

/// The line that says a run could not see every check, if it could not.
fn partial_line(verdicts: &[CheckVerdict], sim_allowed: bool) -> Option<String> {
    let seen = verdicts
        .iter()
        .filter(|v| v.status != CheckStatus::NotObservable)
        .count();
    let all_passed = verdicts.iter().all(|v| v.status != CheckStatus::Fail);
    let full = if sim_allowed {
        " Full check: ter run --sim"
    } else {
        ""
    };
    if seen == verdicts.len() || !all_passed {
        None
    } else if seen == 0 {
        Some(format!(
            "No check can be seen on this setup, so the run is posted as not run.{full}"
        ))
    } else {
        Some(format!(
            "{seen} of {} checks seen on this setup, all passed.{full}",
            verdicts.len()
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
            print_verdicts(verdicts, sim_allowed)
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
    fn a_partial_pass_says_so_and_points_at_the_full_check() {
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
            partial_line(&some, true).unwrap(),
            "1 of 3 checks seen on this setup, all passed. Full check: ter run --sim"
        );
        assert_eq!(
            partial_line(&some, false).unwrap(),
            "1 of 3 checks seen on this setup, all passed."
        );
        let none = [v("blink", NotObservable)];
        assert!(
            partial_line(&none, true)
                .unwrap()
                .starts_with("No check can be seen")
        );
        assert_eq!(partial_line(&[v("a", Pass)], true), None);
        assert_eq!(
            partial_line(&[v("a", Fail), v("b", NotObservable)], true),
            None
        );
    }

    #[test]
    fn each_mode_has_its_venue() {
        assert_eq!(venue_for("simulation", None).unwrap(), "wokwi");
        assert_eq!(venue_for("hardware", None).unwrap(), "local");
        assert_eq!(venue_for("simulation", Some("wokwi")).unwrap(), "wokwi");
        let err = venue_for("simulation", Some("sim86")).unwrap_err();
        assert_eq!(err.code, "venue_unavailable");
        assert!(err.message.contains("wokwi"), "{}", err.message);
        assert!(venue_for("hardware", Some("wokwi")).is_err());
    }
}
