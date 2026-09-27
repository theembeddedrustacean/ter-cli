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
use ter_telemetry::wokwi::Wokwi;
use ter_telemetry::{Capture, Venue, rfc3339_utc};

use crate::build::cargo_build;
use crate::config::Config;
use crate::output::{CliError, print_json};
use crate::record::{Infra, Outcome, RunContext, assemble, file_sha256};
use crate::session::Session;
use crate::sim;
use crate::telemetry::{RUN_CHECK, bad_check, check_recording};

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

/// What the run does after the build, settled before it.
enum Plan {
    /// `--no-check`.
    BuildOnly,
    /// No check.yaml: build only, whatever the mode.
    NoCheck,
    Wokwi(Box<Simulated>),
}

struct Simulated {
    file: CheckFile,
    text: String,
    ready: sim::Ready,
}

/// The default venue of each mode, and the ones this ter can run.
fn venue_for(mode: &str, asked: Option<&str>) -> Result<&'static str, CliError> {
    let (default, known): (&str, &[&'static str]) = match mode {
        "simulation" => ("wokwi", &["wokwi"]),
        _ => ("local", &["local"]),
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
    } else if venue == "local" {
        return Err(CliError::new(
            "venue_unavailable",
            "This ter cannot run exercises on a board yet. `ter run --sim` runs it in Wokwi; `ter run --no-check` builds it and records the attempt.",
        ));
    } else {
        let text = std::fs::read_to_string(&check_path).map_err(|e| {
            CliError::new(
                "io_error",
                format!("Could not read {}: {e}", check_path.display()),
            )
        })?;
        let file = CheckFile::parse(&text).map_err(|e| bad_check(&check_path, e))?;
        let ready = sim::ready(sim::plan(&dir, &ter, &file)?)?;
        Plan::Wokwi(Box::new(Simulated { file, text, ready }))
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

    let (number, recording) = new_recording(&dir)?;
    if !json {
        let how = match plan {
            Plan::BuildOnly => "build only",
            Plan::NoCheck => "build only, no check.yaml",
            Plan::Wokwi(_) => "then Wokwi",
        };
        eprintln!("Building {} ({mode}, {how})", ter.exercise);
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
        (Plan::Wokwi(_), None) => Outcome::Infra {
            failure: Infra::VenueUnavailable,
            output: "cargo built no program to run".into(),
        },
        (Plan::Wokwi(sim), Some(elf)) => {
            let Simulated { file, text, ready } = sim.as_ref();
            if !json {
                eprintln!(
                    "Running {} ms in Wokwi (a few seconds more than that)",
                    file.timeout_ms
                );
            }
            let t = Instant::now();
            let capture = Capture {
                target: ter.target.clone(),
                cli_version: client.cli_version().to_string(),
                elf_sha256: elf_sha256.clone().unwrap_or_default(),
                ..Capture::default()
            };
            let outcome = simulate(ready, file, text, elf, capture, &recording)?;
            ran_in = Some(t.elapsed());
            outcome
        }
    };
    let record = assemble(
        RunContext {
            ter: &ter,
            mode: &mode,
            venue: matches!(plan, Plan::Wokwi(_)).then_some(venue),
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
    if json {
        print_json(&answer_json(
            &record,
            &answer,
            &outcome,
            &recording,
            failure.as_ref(),
        ));
    } else {
        print_text(&record, &answer, &outcome, build.duration, ran_in);
    }
    match failure {
        Some(mut err) => {
            err.in_json_answer = json;
            Err(err)
        }
        None => Ok(()),
    }
}

/// Run the program in Wokwi, record the run into `recording`, and judge
/// the recording.
fn simulate(
    ready: &sim::Ready,
    file: &CheckFile,
    text: &str,
    elf: &Path,
    capture: Capture,
    recording: &Path,
) -> Result<Outcome, CliError> {
    let mut wokwi = Wokwi {
        cli: ready.cli.clone(),
        token: ready.token.clone(),
        run_dir: recording.to_path_buf(),
        plan: ready.plan.clone(),
        timeout_ms: file.timeout_ms,
        capture,
    };
    // wokwi-cli paces the simulation step by step over the network; allow
    // it far longer than the simulated time.
    let budget = Duration::from_secs(120) + Duration::from_millis(file.timeout_ms * 20);
    let recorded = wokwi
        .prepare(elf)
        .and_then(|()| wokwi.run(&file.stimuli(), budget));
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

fn print_verdicts(verdicts: &[CheckVerdict]) {
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
    let seen = verdicts
        .iter()
        .filter(|v| v.status != CheckStatus::NotObservable)
        .count();
    let all_passed = verdicts.iter().all(|v| v.status != CheckStatus::Fail);
    if seen < verdicts.len() && seen > 0 && all_passed {
        println!(
            "{seen} of {} checks seen on this setup, all passed. Full check: ter run --sim",
            verdicts.len()
        );
    }
}

fn print_text(
    record: &RunRecord,
    answer: &RunAnswer,
    outcome: &Outcome,
    built_in: Duration,
    ran_in: Option<Duration>,
) {
    if record.build_status == "passed" {
        print!("Built in {:.1} s.", built_in.as_secs_f64());
        if let Some(t) = ran_in {
            print!(" Ran in Wokwi in {:.1} s.", t.as_secs_f64());
        }
        println!();
    } else {
        println!("Build failed.");
    }
    match outcome {
        Outcome::Built => println!("Not checked: --no-check builds only."),
        Outcome::NoCheck => println!("{NO_CHECK_NOTE}"),
        Outcome::Checked { verdicts, .. } => print_verdicts(verdicts),
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
