//! `ter run`: build an exercise, record the run and post it to the site.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};
use ter_sdk::project::{Layout, TerToml, new_recording};
use ter_sdk::run::{LastRun, RunAnswer, RunRecord, rfc3339_utc};

use crate::build::cargo_build;
use crate::config::Config;
use crate::output::{CliError, print_json};
use crate::record::{Outcome, RunContext, assemble, file_sha256};
use crate::session::Session;

pub struct RunArgs {
    pub dir: Option<PathBuf>,
    /// `--sim` or `--hw`: the mode for this run instead of ter.toml's.
    pub mode: Option<&'static str>,
    pub no_check: bool,
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
    if !args.no_check {
        return Err(CliError::new(
            "venue_unavailable",
            "This ter cannot run exercises on a board or in a simulator yet. `ter run --no-check` builds the exercise and records the attempt.",
        ));
    }

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
        eprintln!("Building {} ({mode}, build only)", ter.exercise);
    }
    let layout = Layout::new(Config::load()?.courses_root);
    let build = cargo_build(&dir, layout.cargo_env(&ter, &dir), !json)?;
    let log_path = recording.join("build.log");
    std::fs::write(&log_path, &build.log).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not write {}: {e}", log_path.display()),
        )
    })?;

    let previous =
        LastRun::load(&dir)?.filter(|last| last.user == user && last.site == client.site_url());
    let elf_sha256 = build.elf.as_deref().map(file_sha256).transpose()?;
    let outcome = if build.ok {
        Outcome::Built
    } else {
        Outcome::BuildFailed { log: build.log }
    };
    let record = assemble(
        RunContext {
            ter: &ter,
            mode: &mode,
            venue: None,
            elf_sha256,
            duration_ms: u64::try_from(build.duration.as_millis()).unwrap_or(u64::MAX),
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

    let failure = (!build.ok).then(|| {
        CliError::new(
            "build_failed",
            format!(
                "The build failed. The errors are above and in {}.",
                log_path.display()
            ),
        )
    });
    if json {
        print_json(&answer_json(&record, &answer, &recording, failure.as_ref()));
    } else {
        print_text(&record, &answer, build.duration.as_secs_f64());
    }
    match failure {
        Some(mut err) => {
            err.in_json_answer = json;
            Err(err)
        }
        None => Ok(()),
    }
}

/// The run record as posted, plus the site's answer, as `ter serve` will
/// return it to the lesson page.
fn answer_json(
    record: &RunRecord,
    answer: &RunAnswer,
    recording: &Path,
    failure: Option<&CliError>,
) -> Value {
    let mut out = serde_json::to_value(record).expect("record serialises");
    out["name"] = json!(answer.name);
    out["site"] = json!({"attempt": answer.attempt, "concept_deltas": answer.concept_deltas});
    out["recording"] = json!(recording);
    if let Some(err) = failure {
        out["error"] = json!({"code": err.code, "message": err.message});
    }
    out
}

fn print_text(record: &RunRecord, answer: &RunAnswer, seconds: f64) {
    if record.build_status == "passed" {
        println!("Built in {seconds:.1} s. Not checked: --no-check builds only.");
    } else {
        println!("Build failed.");
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
    if record.build_status == "failed" {
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
