//! What a run posts: the outcome projected onto the site's run record.
//!
//! The site's enum has `passed`, `failed` and `not_run`. A failure of the
//! venue (a board that went away, a flash that did not take, a simulator
//! account over its quota) is not the learner's, so it is never `failed`:
//! it posts `not_run`, with its code as the first line of the transcript.

use std::path::Path;

use sha2::{Digest, Sha256};
use ter_check::{CheckStatus, CheckVerdict};
use ter_sdk::project::TerToml;
use ter_sdk::run::{RunRecord, TAIL_LIMIT, tail, transcript_with_code};
use ter_telemetry::VenueFailure;

use crate::output::CliError;

/// A failure of where the program runs, not of the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Infra {
    /// The venue is not reachable or died mid-capture; also a provider
    /// account over its quota.
    VenueUnavailable,
    BenchOffline,
    FlashFailed,
}

impl From<VenueFailure> for Infra {
    fn from(f: VenueFailure) -> Self {
        match f {
            VenueFailure::Unavailable => Infra::VenueUnavailable,
            VenueFailure::BenchOffline => Infra::BenchOffline,
            VenueFailure::FlashFailed => Infra::FlashFailed,
        }
    }
}

impl Infra {
    #[cfg(test)]
    pub const ALL: [Infra; 3] = [
        Infra::VenueUnavailable,
        Infra::BenchOffline,
        Infra::FlashFailed,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Infra::VenueUnavailable => "venue_unavailable",
            Infra::BenchOffline => "bench_offline",
            Infra::FlashFailed => "flash_failed",
        }
    }
}

/// How a run ended, before it is written as a run record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Cargo failed; `log` is its output.
    BuildFailed { log: String },
    /// It built and nothing ran (`--no-check`).
    Built,
    /// It built, and the exercise has no check.yaml to run it against.
    NoCheck,
    /// It built, and the venue failed; `output` is what it said.
    Infra { failure: Infra, output: String },
    /// It ran and was judged. `transcript` is what the module printed.
    Checked {
        verdicts: Vec<CheckVerdict>,
        transcript: String,
    },
}

/// Everything about a run that is not its outcome.
pub struct RunContext<'a> {
    pub ter: &'a TerToml,
    pub mode: &'a str,
    pub venue: Option<&'a str>,
    pub elf_sha256: Option<String>,
    pub duration_ms: u64,
    /// Hints taken on the previous run of this exercise.
    pub hints_used: u32,
    pub cli_version: &'a str,
}

/// The run record for `outcome`.
pub fn assemble(ctx: RunContext<'_>, outcome: &Outcome) -> RunRecord {
    let mut record = RunRecord {
        exercise_id: ctx.ter.exercise.clone(),
        target: ctx.ter.target.clone(),
        mode: ctx.mode.to_string(),
        venue: ctx.venue.map(str::to_string),
        build_status: "passed".into(),
        check_status: "not_run".into(),
        elf_sha256: ctx.elf_sha256,
        duration_ms: ctx.duration_ms,
        hints_used: ctx.hints_used,
        cli_version: ctx.cli_version.to_string(),
        ..RunRecord::default()
    };
    match outcome {
        Outcome::BuildFailed { log } => {
            record.build_status = "failed".into();
            record.compiler_tail = Some(tail(log, TAIL_LIMIT).to_string());
            record.elf_sha256 = None;
        }
        Outcome::Built => {}
        Outcome::NoCheck => {
            record.checks_seen = Some(0);
            record.checks_total = Some(0);
        }
        Outcome::Infra { failure, output } => {
            record.transcript_tail = Some(transcript_with_code(failure.code(), output));
        }
        Outcome::Checked {
            verdicts,
            transcript,
        } => {
            let seen: Vec<&CheckVerdict> = verdicts
                .iter()
                .filter(|v| v.status != CheckStatus::NotObservable)
                .collect();
            record.checks_seen = Some(count(seen.len()));
            record.checks_total = Some(count(verdicts.len()));
            record.transcript_tail = Some(tail(transcript, TAIL_LIMIT).to_string());
            record.check_status =
                if let Some(v) = seen.iter().find(|v| v.status == CheckStatus::Fail) {
                    record.first_failure = Some(first_failure(v));
                    "failed"
                } else if seen.is_empty() {
                    "not_run"
                } else {
                    // Some unseen with every seen one passed is a partial pass:
                    // the counts tell the site not to count it.
                    "passed"
                }
                .into();
        }
    }
    record
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The run record's `first_failure`: `{id, expected, observed}` as JSON.
pub fn first_failure(v: &CheckVerdict) -> String {
    serde_json::json!({"id": v.id, "expected": v.expected, "observed": v.observed}).to_string()
}

/// sha256 of the built program, lowercase hex.
pub fn file_sha256(path: &Path) -> Result<String, CliError> {
    let bytes = std::fs::read(path).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not read {}: {e}", path.display()),
        )
    })?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ter() -> TerToml {
        TerToml {
            exercise: "gpio-blinky--xiao-esp32c3-nostd".into(),
            course: "esp-gpio".into(),
            target: "xiao-esp32c3-nostd".into(),
            modes: vec!["hardware".into(), "simulation".into()],
            mode: "hardware".into(),
            venue: None,
            fetched_sha256: String::new(),
            pins: Default::default(),
        }
    }

    fn ctx(ter: &TerToml) -> RunContext<'_> {
        RunContext {
            ter,
            mode: "simulation",
            venue: None,
            elf_sha256: Some("ab".repeat(32)),
            duration_ms: 1234,
            hints_used: 2,
            cli_version: "0.1.0",
        }
    }

    #[test]
    fn a_build_only_run_is_not_run() {
        let ter = ter();
        let r = assemble(ctx(&ter), &Outcome::Built);
        assert_eq!(r.exercise_id, ter.exercise);
        assert_eq!(r.target, ter.target);
        assert_eq!(r.mode, "simulation");
        assert_eq!(
            (r.build_status.as_str(), r.check_status.as_str()),
            ("passed", "not_run")
        );
        assert_eq!(r.elf_sha256, Some("ab".repeat(32)));
        assert_eq!((r.duration_ms, r.hints_used), (1234, 2));
        assert_eq!(r.cli_version, "0.1.0");
        assert_eq!(r.compiler_tail, None);
        assert_eq!(r.first_failure, None);
    }

    #[test]
    fn a_failed_build_carries_the_end_of_the_log_and_no_elf() {
        let ter = ter();
        let log = format!(
            "{}error[E0425]: cannot find value `led` in this scope\n",
            "   Compiling dep v1.0.0\n".repeat(400)
        );
        let r = assemble(ctx(&ter), &Outcome::BuildFailed { log });
        assert_eq!(r.build_status, "failed");
        assert_eq!(r.check_status, "not_run");
        let t = r.compiler_tail.unwrap();
        assert!(t.chars().count() <= TAIL_LIMIT);
        assert!(t.ends_with("cannot find value `led` in this scope\n"));
        assert_eq!(r.elf_sha256, None);
    }

    #[test]
    fn every_infra_failure_is_not_run_never_failed() {
        let ter = ter();
        for failure in Infra::ALL {
            let r = assemble(
                ctx(&ter),
                &Outcome::Infra {
                    failure,
                    output: "boot: ok\n".repeat(1000),
                },
            );
            assert_eq!(r.build_status, "passed", "{failure:?}");
            assert_eq!(r.check_status, "not_run", "{failure:?}");
            assert_eq!(r.first_failure, None, "{failure:?}");
            let t = r.transcript_tail.unwrap();
            assert_eq!(t.lines().next(), Some(failure.code()));
            assert!(t.chars().count() <= TAIL_LIMIT);
        }
    }

    fn v(id: &str, status: CheckStatus) -> CheckVerdict {
        CheckVerdict {
            id: id.into(),
            status,
            expected: format!("{id} expected"),
            observed: format!("{id} observed"),
        }
    }

    fn checked(verdicts: Vec<CheckVerdict>) -> RunRecord {
        let ter = ter();
        assemble(
            ctx(&ter),
            &Outcome::Checked {
                verdicts,
                transcript: "Hello world!\n".into(),
            },
        )
    }

    use CheckStatus::{Fail, NotObservable, Pass};

    #[test]
    fn all_seen_and_passed_is_passed() {
        let r = checked(vec![v("a", Pass), v("b", Pass)]);
        assert_eq!(r.check_status, "passed");
        assert_eq!((r.checks_seen, r.checks_total), (Some(2), Some(2)));
        assert_eq!(r.first_failure, None);
        assert_eq!(r.transcript_tail.as_deref(), Some("Hello world!\n"));
    }

    #[test]
    fn the_first_seen_failure_is_the_first_failure() {
        let r = checked(vec![
            v("a", Pass),
            v("b", NotObservable),
            v("c", Fail),
            v("d", Fail),
        ]);
        assert_eq!(r.check_status, "failed");
        let ff: serde_json::Value =
            serde_json::from_str(r.first_failure.as_deref().unwrap()).unwrap();
        assert_eq!(
            ff,
            serde_json::json!({"id": "c", "expected": "c expected", "observed": "c observed"})
        );
        assert_eq!((r.checks_seen, r.checks_total), (Some(3), Some(4)));
    }

    #[test]
    fn some_unseen_all_seen_passed_is_a_partial_pass() {
        let r = checked(vec![v("a", Pass), v("b", NotObservable)]);
        assert_eq!(r.check_status, "passed");
        assert_eq!((r.checks_seen, r.checks_total), (Some(1), Some(2)));
    }

    #[test]
    fn nothing_seen_is_not_run() {
        let r = checked(vec![v("a", NotObservable)]);
        assert_eq!(r.check_status, "not_run");
        assert_eq!((r.checks_seen, r.checks_total), (Some(0), Some(1)));
    }

    #[test]
    fn no_check_yaml_is_not_run_with_no_checks() {
        let ter = ter();
        let r = assemble(ctx(&ter), &Outcome::NoCheck);
        assert_eq!(
            (r.build_status.as_str(), r.check_status.as_str()),
            ("passed", "not_run")
        );
        assert_eq!((r.checks_seen, r.checks_total), (Some(0), Some(0)));
    }

    #[test]
    fn elf_hash_is_sha256_hex() {
        let dir = tempfile::tempdir().unwrap();
        let elf = dir.path().join("elf");
        std::fs::write(&elf, b"abc").unwrap();
        assert_eq!(
            file_sha256(&elf).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
