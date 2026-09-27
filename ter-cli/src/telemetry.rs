//! `ter telemetry check`: judge a recorded run against a check.yaml, with
//! no venue and no account.
//!
//! `ter run` records first and then judges its own recording with the same
//! function, so re-checking a run folder with its check.yaml gives its
//! `check.toml` back byte for byte.

use std::path::{Path, PathBuf};

use ter_check::{CheckFile, CheckStatus, Report};
use ter_telemetry::Recording;

use crate::output::{CliError, print_json};

/// The check.yaml a run was judged against, kept in its run folder.
pub const RUN_CHECK: &str = "check.yaml";

pub fn bad_check(path: &Path, e: impl std::fmt::Display) -> CliError {
    CliError::new(
        "bad_check",
        format!("{} is not a valid check: {e}", path.display()),
    )
}

fn read(path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not read {}: {e}", path.display()),
        )
    })
}

/// The report for the recording at `recording` (a run folder or its
/// `events.jsonl`), judged against `check_yaml`.
pub fn check_recording(
    recording: &Path,
    check_yaml: &str,
    check_path: &Path,
) -> Result<(Report, Recording), CliError> {
    let rec = Recording::read(recording)
        .map_err(|e| CliError::new("bad_recording", format!("Not a ter recording: {e}")))?;
    let report = ter_check::check(check_yaml, &rec).map_err(|e| bad_check(check_path, e))?;
    Ok((report, rec))
}

pub fn check(recording: PathBuf, check: PathBuf, json: bool) -> Result<(), CliError> {
    let text = read(&check)?;
    let (report, _) = check_recording(&recording, &text, &check)?;
    if json {
        print_json(&report);
    } else {
        print!("{}", report.to_toml());
        let file = CheckFile::parse(&text).map_err(|e| bad_check(&check, e))?;
        for (c, v) in file.checks.iter().zip(&report.verdicts) {
            if let Some(h) = ter_check::hints::hint(c, v) {
                eprintln!("hint: {h}");
            }
        }
    }
    if report
        .verdicts
        .iter()
        .any(|v| v.status == CheckStatus::Fail)
    {
        let mut err = CliError::new("check_failed", "A check failed; the verdicts are above.");
        err.in_json_answer = json;
        return Err(err);
    }
    Ok(())
}
