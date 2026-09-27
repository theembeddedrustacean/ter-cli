//! The `check.yaml` evaluator.
//!
//! `ter-check` reads a recorded event stream and a `check.yaml` and returns
//! one [`CheckVerdict`] per check. It knows nothing about the site, the
//! learner's source or where the run happened: turning verdicts into a run
//! record is the caller's job.
//!
//! This crate must never depend on anything that reaches the network; the
//! local check enforces it.

use serde::{Deserialize, Serialize};
use ter_telemetry::Recording;

pub mod eval;
pub mod hints;
pub mod report;
pub mod spec;

pub use eval::evaluate;
pub use report::{REPORT_FILE, Report};
pub use spec::CheckFile;

/// The outcome of one check in `check.yaml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// The venue and its instruments could not see what this check needs.
    NotObservable,
}

/// The verdict for one check, in the order the checks appear in `check.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckVerdict {
    pub id: String,
    pub status: CheckStatus,
    pub expected: String,
    pub observed: String,
}

/// Judge `rec` against the text of a `check.yaml`: the report `ter run`
/// writes and `ter telemetry check` prints.
pub fn check(check_yaml: &str, rec: &Recording) -> Result<Report, String> {
    let file = CheckFile::parse(check_yaml)?;
    let verdicts = evaluate(&file, rec);
    Ok(Report::new(&rec.capture, check_yaml.as_bytes(), verdicts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_serialises_with_snake_case_status() {
        let v = CheckVerdict {
            id: "led-blinks".into(),
            status: CheckStatus::NotObservable,
            expected: "LED toggles 2 times per second".into(),
            observed: "no pin events on this venue".into(),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["status"], "not_observable");
        let back: CheckVerdict = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }
}
