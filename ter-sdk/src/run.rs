//! Run records and hints: what `ter run` posts, what the site answers, and
//! what `ter` remembers about the last run of an exercise.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::project::{ProjectError, STATE_DIR};

/// The site keeps this many characters of each tail and cuts the rest from
/// the end, so `ter` sends at most this many, from the end of the text.
pub const TAIL_LIMIT: usize = 4000;

/// One run as `run` takes it. Fields left `None` are not sent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunRecord {
    pub exercise_id: String,
    pub target: String,
    /// `hardware` or `simulation`.
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    /// `passed` or `failed`.
    pub build_status: String,
    /// `passed`, `failed` or `not_run`.
    pub check_status: String,
    /// `{id, expected, observed}` as a JSON string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compiler_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elf_sha256: Option<String>,
    pub duration_ms: u64,
    pub hints_used: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_seen: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_total: Option<u32>,
    pub cli_version: String,
}

/// The site's answer to `run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunAnswer {
    /// The run's name on the site, which `hint` takes.
    pub name: String,
    /// Per user per exercise, counted by the site.
    pub attempt: u32,
    #[serde(default)]
    pub concept_deltas: Vec<ConceptDelta>,
    /// Whether this run completed the lesson. False after a pass on the
    /// learner's own board; absent from sites that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson_completed: Option<bool>,
}

/// How one concept's mastery moved on a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConceptDelta {
    pub concept: String,
    #[serde(default)]
    pub label: Option<String>,
    pub before: f64,
    pub after: f64,
    #[serde(default)]
    pub correct: bool,
    #[serde(default)]
    pub weight: f64,
}

/// A learner's file as `hint` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HintFile {
    pub path: String,
    pub content: String,
}

/// One exchange with the learner's own hint model, as `hint_exchange`
/// takes it: what was sent and what came back. Never the provider key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintExchange {
    /// The run the hint was for.
    pub run: String,
    pub exercise_id: String,
    /// The provider and model the learner configured.
    pub provider: String,
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub answer: String,
    pub asked_at: String,
    pub cli_version: String,
}

/// The site's answer to `hint`. `number` 0 means there is no hint: the run
/// passed, or the site has no hint source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintAnswer {
    pub number: u32,
    pub text: String,
    #[serde(default)]
    pub exhausted: bool,
}

/// The last `limit` characters of `text` at most, cut on a line break when
/// one is near so the tail starts with a whole line.
pub fn tail(text: &str, limit: usize) -> &str {
    let chars = text.chars().count();
    if chars <= limit {
        return text;
    }
    let start = text
        .char_indices()
        .nth(chars - limit)
        .map(|(i, _)| i)
        .expect("chars > limit");
    let cut = &text[start..];
    match cut.find('\n') {
        // Drop the partial first line, unless that would drop most of it.
        Some(nl) if nl < cut.len() / 4 => &cut[nl + 1..],
        _ => cut,
    }
}

/// A transcript whose first line is `code`, within [`TAIL_LIMIT`]: how an
/// infrastructure failure is told apart from the program's own output.
pub fn transcript_with_code(code: &str, output: &str) -> String {
    let room = TAIL_LIMIT.saturating_sub(code.chars().count() + 1);
    let rest = tail(output, room);
    if rest.is_empty() {
        code.to_string()
    } else {
        format!("{code}\n{rest}")
    }
}

/// The hints taken on one run, as the site counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HintCount {
    /// Hints the site recorded against the run.
    pub used: u32,
    /// The ladder has run out; later answers are the walk-through offer,
    /// which the site does not record.
    pub exhausted: bool,
}

impl HintCount {
    /// Count one answer from `hint`.
    pub fn record(&mut self, answer: &HintAnswer) {
        if answer.number == 0 || self.exhausted {
            return;
        }
        self.used = self.used.max(answer.number);
        self.exhausted = answer.exhausted;
    }
}

/// What `ter` remembers about the last posted run of an exercise, in
/// `<exercise>/.ter/last-run.json`. `ter ex clean` keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    /// The account that posted it.
    pub user: String,
    pub site: String,
    pub record: RunRecord,
    pub answer: RunAnswer,
    /// When it was posted, RFC 3339 UTC.
    pub posted_at: String,
    /// The run's recording folder, relative to the exercise.
    pub recording: PathBuf,
    #[serde(default)]
    pub hints: HintCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hint: Option<HintAnswer>,
}

const LAST_RUN: &str = "last-run.json";

impl LastRun {
    pub fn path(exercise_dir: &Path) -> PathBuf {
        exercise_dir.join(STATE_DIR).join(LAST_RUN)
    }

    /// `None` when no run has been posted from this folder.
    pub fn load(exercise_dir: &Path) -> Result<Option<Self>, ProjectError> {
        let path = Self::path(exercise_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ProjectError::io("read", &path, e)),
        };
        serde_json::from_str(&text).map(Some).map_err(|e| {
            ProjectError::new(
                "config_error",
                format!(
                    "{} is not valid ({e}); delete it and run again.",
                    path.display()
                ),
            )
        })
    }

    pub fn save(&self, exercise_dir: &Path) -> Result<(), ProjectError> {
        let path = Self::path(exercise_dir);
        let dir = path.parent().expect("has a parent");
        std::fs::create_dir_all(dir).map_err(|e| ProjectError::io("create", dir, e))?;
        let body = serde_json::to_string_pretty(self).expect("last run serialises");
        std::fs::write(&path, body).map_err(|e| ProjectError::io("write", &path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_left_alone() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("", 10), "");
    }

    #[test]
    fn tail_keeps_the_end_within_the_limit() {
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let t = tail(&text, TAIL_LIMIT);
        assert!(t.chars().count() <= TAIL_LIMIT);
        assert!(t.ends_with("line 1999\n"));
        assert!(
            t.starts_with("line "),
            "starts on a whole line: {:?}",
            &t[..20]
        );
    }

    #[test]
    fn tail_counts_characters_not_bytes() {
        let text = "é".repeat(5000);
        let t = tail(&text, TAIL_LIMIT);
        assert_eq!(t.chars().count(), TAIL_LIMIT);
    }

    #[test]
    fn one_long_line_is_cut_mid_line() {
        let text = "x".repeat(10_000);
        assert_eq!(tail(&text, TAIL_LIMIT).len(), TAIL_LIMIT);
    }

    #[test]
    fn transcript_code_stays_first_and_fits() {
        let output = "y\n".repeat(5000);
        let t = transcript_with_code("venue_unavailable", &output);
        assert!(t.starts_with("venue_unavailable\n"));
        assert!(t.chars().count() <= TAIL_LIMIT);
        assert_eq!(transcript_with_code("flash_failed", ""), "flash_failed");
    }

    fn hint(number: u32, exhausted: bool) -> HintAnswer {
        HintAnswer {
            number,
            text: "…".into(),
            exhausted,
        }
    }

    #[test]
    fn hints_count_as_the_site_records_them() {
        let mut count = HintCount::default();
        count.record(&hint(1, false));
        count.record(&hint(2, false));
        assert_eq!(count.used, 2);
        // The last rung is recorded and says the ladder is done.
        count.record(&hint(3, true));
        assert_eq!(
            count,
            HintCount {
                used: 3,
                exhausted: true
            }
        );
        // Asking again gets the walk-through offer, not recorded.
        count.record(&hint(4, true));
        count.record(&hint(5, true));
        assert_eq!(count.used, 3);
    }

    #[test]
    fn no_hint_spends_nothing() {
        let mut count = HintCount::default();
        count.record(&hint(0, false));
        assert_eq!(count, HintCount::default());
    }

    #[test]
    fn unset_fields_are_not_sent() {
        let record = RunRecord {
            exercise_id: "x".into(),
            target: "t".into(),
            mode: "hardware".into(),
            build_status: "passed".into(),
            check_status: "not_run".into(),
            cli_version: "0.1.0".into(),
            ..RunRecord::default()
        };
        let v = serde_json::to_value(&record).unwrap();
        for absent in [
            "venue",
            "first_failure",
            "compiler_tail",
            "elf_sha256",
            "checks_total",
        ] {
            assert!(v.get(absent).is_none(), "{absent} sent: {v}");
        }
        assert_eq!(v["hints_used"], 0);
    }

    #[test]
    fn answer_reads_the_sites_concept_deltas() {
        let a: RunAnswer = serde_json::from_value(serde_json::json!({
            "name": "a1b2c3", "attempt": 2,
            "concept_deltas": [{"concept": "gpio-output", "label": "GPIO output",
                "before": 0.3, "after": 0.26, "correct": false, "weight": 0.5}]
        }))
        .unwrap();
        assert_eq!(a.attempt, 2);
        assert_eq!(a.concept_deltas[0].label.as_deref(), Some("GPIO output"));
        assert_eq!(a.lesson_completed, None);
        let a: RunAnswer = serde_json::from_value(serde_json::json!({
            "name": "a1b2c3", "attempt": 3, "lesson_completed": false
        }))
        .unwrap();
        assert_eq!(a.lesson_completed, Some(false));
    }

    #[test]
    fn last_run_round_trips_in_the_state_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(LastRun::load(dir.path()).unwrap(), None);
        let last = LastRun {
            user: "learner@example.com".into(),
            site: "https://example.com".into(),
            record: RunRecord::default(),
            answer: RunAnswer {
                name: "r1".into(),
                attempt: 1,
                concept_deltas: vec![],
                lesson_completed: None,
            },
            posted_at: "2026-09-27T12:00:00Z".into(),
            recording: ".runs/1".into(),
            hints: HintCount::default(),
            last_hint: None,
        };
        last.save(dir.path()).unwrap();
        assert!(dir.path().join(".ter/last-run.json").is_file());
        assert_eq!(LastRun::load(dir.path()).unwrap(), Some(last));
    }
}
