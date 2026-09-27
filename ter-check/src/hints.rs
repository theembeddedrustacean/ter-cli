//! Layer 1 hints: a template per check kind, read off a failed verdict.
//!
//! The site composes the same hints from the run record; these are for
//! `ter telemetry check`, which works with no account.

use crate::spec::{Check, What};
use crate::{CheckStatus, CheckVerdict};

/// A hint for a failed check: the evidence, then one question. `None`
/// for a check that did not fail.
pub fn hint(check: &Check, verdict: &CheckVerdict) -> Option<String> {
    if verdict.status != CheckStatus::Fail {
        return None;
    }
    let evidence = format!(
        "{}: expected {}, observed {}.",
        verdict.id, verdict.expected, verdict.observed
    );
    let question = match &check.what {
        What::SerialContains { text, .. } => format!(
            "Does your program print {text:?} exactly, spelling and spaces included, and does it get to that line in time? A panic or a loop that never ends stops the output early."
        ),
        What::Is { pin, level, .. } => format!(
            "Which line of your program sets {pin} {level}, and has it run by the time the check looks? Follow the program from reset to that moment."
        ),
        What::TogglesPerS { pin, .. } => format!(
            "A toggle is one change of level, so a pin that blinks once a second toggles twice a second. How long does {pin} stay at each level in your loop?"
        ),
        What::EdgeCount { pin, .. } => format!(
            "The check counts every change of {pin} in the window. Does your program change it once per event, or on every pass of the loop while a condition holds?"
        ),
        What::PulseWidthMs { pin, .. } => format!(
            "A pulse is how long {pin} stays high before it goes low again. Which wait in your program sets that time?"
        ),
    };
    Some(format!("{evidence} {question}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::CheckFile;

    #[test]
    fn a_failed_check_gets_its_evidence_and_a_question() {
        let f = CheckFile::parse(
            "timeout_ms: 1000\nassert:\n  - id: blink\n    pin: user_led\n    toggles_per_s: {min: 1.8, max: 2.2}\n    window_ms: [0, 1000]\n",
        )
        .unwrap();
        let v = CheckVerdict {
            id: "blink".into(),
            status: CheckStatus::Fail,
            expected: "1.8 to 2.2 toggles per second between 0 and 1000 ms".into(),
            observed: "1 toggles per second (1 level change)".into(),
        };
        let h = hint(&f.checks[0], &v).unwrap();
        assert!(
            h.starts_with("blink: expected 1.8 to 2.2 toggles per second"),
            "{h}"
        );
        assert!(h.contains("toggles twice a second"), "{h}");
        let pass = CheckVerdict {
            status: CheckStatus::Pass,
            ..v
        };
        assert_eq!(hint(&f.checks[0], &pass), None);
    }
}
