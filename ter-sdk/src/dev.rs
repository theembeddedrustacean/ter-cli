//! Exercises from a local curriculum checkout, for authors (`--dev`).
//!
//! The checkout's own `tools/exercise_sync.py --print` resolves the
//! exercise, so what `ter` writes is exactly what the sync would send the
//! site: the same layers, generator and substitutions.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::exercise::Exercise;
use crate::project::ProjectError;

const SYNC_TOOL: &str = "tools/exercise_sync.py";
const SYNC_STATE: &str = ".ter-sync/state.json";
const TARGETS: &str = "targets";

/// An exercise resolved from a checkout, with what the site would have
/// said about its course and modes.
#[derive(Debug)]
pub struct DevExercise {
    pub exercise: Exercise,
    /// The site's course id when the checkout has been synced, else the
    /// course's folder name in the checkout.
    pub course: String,
    pub modes: Vec<String>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(flatten)]
    exercise: Exercise,
    #[serde(default)]
    lesson: Option<String>,
    #[serde(default = "one")]
    allow_hardware: u8,
    #[serde(default = "one")]
    allow_simulation: u8,
}

fn one() -> u8 {
    1
}

fn dev_error(message: impl Into<String>) -> ProjectError {
    ProjectError {
        code: "dev_error",
        message: message.into(),
    }
}

/// Resolve `exercise_id` (`<exercise>--<target>`) from the checkout at
/// `curriculum`.
pub fn load(curriculum: &Path, exercise_id: &str) -> Result<DevExercise, ProjectError> {
    let (slug, _target) = exercise_id.split_once("--").ok_or_else(|| {
        dev_error(format!(
            "{exercise_id:?} is not an exercise id of the form <exercise>--<target>."
        ))
    })?;
    if !curriculum.join(SYNC_TOOL).is_file() {
        return Err(dev_error(format!(
            "{} is not a curriculum checkout (no {SYNC_TOOL}).",
            curriculum.display()
        )));
    }
    let out = Command::new("python3")
        .arg(SYNC_TOOL)
        .args(["--exercise", slug, "--print"])
        .current_dir(curriculum)
        .output()
        .map_err(|e| dev_error(format!("Could not run python3 {SYNC_TOOL}: {e}")))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        });
        return Err(dev_error(format!(
            "{SYNC_TOOL} --exercise {slug} failed: {}",
            tail(text.trim(), 1200)
        )));
    }
    let payloads: Vec<Payload> = serde_json::from_slice(&out.stdout)
        .map_err(|e| dev_error(format!("{SYNC_TOOL} --print gave unexpected output: {e}")))?;
    let targets: Vec<String> = payloads
        .iter()
        .map(|p| p.exercise.exercise_id.clone())
        .collect();
    let payload = payloads
        .into_iter()
        .find(|p| p.exercise.exercise_id == exercise_id)
        .ok_or_else(|| {
            dev_error(format!(
                "{exercise_id} is not in the checkout; {slug} has {}.",
                if targets.is_empty() {
                    "no targets".into()
                } else {
                    targets.join(", ")
                }
            ))
        })?;

    let state = std::fs::read(curriculum.join(SYNC_STATE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok())
        .unwrap_or_default();
    let course = course_of(&state, payload.lesson.as_deref()).ok_or_else(|| {
        dev_error(format!(
            "Could not tell which course {exercise_id} belongs to from {SYNC_STATE}."
        ))
    })?;
    let modes = [
        (payload.allow_hardware, "hardware"),
        (payload.allow_simulation, "simulation"),
    ]
    .into_iter()
    .filter(|(allowed, _)| *allowed != 0)
    .map(|(_, mode)| mode.to_string())
    .collect();
    let mut exercise = payload.exercise;
    if exercise.pins.is_empty() {
        exercise.pins = target_pins(curriculum, &exercise.target)?;
    }
    Ok(DevExercise {
        exercise,
        course,
        modes,
    })
}

/// The pin map in the checkout's `targets/<target>/target.yaml`.
fn target_pins(curriculum: &Path, target: &str) -> Result<BTreeMap<String, String>, ProjectError> {
    #[derive(Deserialize)]
    struct Target {
        #[serde(default, deserialize_with = "crate::exercise::pin_map")]
        pins: BTreeMap<String, String>,
    }
    let path = curriculum.join(TARGETS).join(target).join("target.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| dev_error(format!("Could not read {}: {e}", path.display())))?;
    let parsed: Target = serde_norway::from_str(&text)
        .map_err(|e| dev_error(format!("{} is not valid: {e}", path.display())))?;
    Ok(parsed.pins)
}

/// The site's course id for the lesson an exercise attaches to. The sync
/// state maps repo keys (`course/chapter/lesson`, and `course` alone) to
/// the site's names.
fn course_of(state: &Map<String, Value>, lesson: Option<&str>) -> Option<String> {
    let lesson = lesson?;
    let key = state
        .iter()
        .find(|(_, v)| v.as_str() == Some(lesson))
        .map(|(k, _)| k)?;
    let repo_course = key.split('/').next()?;
    Some(
        state
            .get(repo_course)
            .and_then(Value::as_str)
            .unwrap_or(repo_course)
            .to_string(),
    )
}

fn tail(text: &str, max: usize) -> &str {
    let start = text.len().saturating_sub(max);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> Map<String, Value> {
        json!({
            "_site": {"x": 1},
            "esp-nostd-gpio": "esp-bare-metal-gpio-programming",
            "esp-nostd-gpio/02-blinky/exercise-gpio-blinky": "0111 Exercise: Heartbeat, not a blink",
            "unsynced/01-a/02-b": "0999 Lesson",
            "Course Chapter/0050 Blinky": {"course": "abc"}
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn course_comes_from_the_sync_state() {
        assert_eq!(
            course_of(&state(), Some("0111 Exercise: Heartbeat, not a blink")).as_deref(),
            Some("esp-bare-metal-gpio-programming")
        );
        assert_eq!(
            course_of(&state(), Some("0999 Lesson")).as_deref(),
            Some("unsynced"),
            "the repo slug when the course itself is not mapped"
        );
        assert_eq!(course_of(&state(), Some("nope")), None);
        assert_eq!(course_of(&state(), None), None);
    }

    #[test]
    fn payload_modes_and_string_fields_parse() {
        let p: Payload = serde_json::from_value(json!({
            "exercise_id": "a--t", "title": "A", "target": "t", "runner": "cargo",
            "kind": "exercise", "lesson": "L", "allow_hardware": 0, "allow_simulation": 1,
            "timeout_seconds": 60, "runner_args": "{}", "toolchain": "{\"triple\": \"x\"}",
            "instructions_md": "# A", "files": [], "hints": "[]", "source_commit": "abc"
        }))
        .unwrap();
        assert_eq!((p.allow_hardware, p.allow_simulation), (0, 1));
        assert_eq!(p.exercise.timeout_seconds, 60);
    }

    #[test]
    fn a_bad_id_or_folder_is_a_dev_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "no-target").unwrap_err().code, "dev_error");
        let err = load(dir.path(), "a--b").unwrap_err();
        assert_eq!(err.code, "dev_error");
        assert!(err.message.contains("not a curriculum checkout"));
    }

    #[test]
    fn pins_come_from_the_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("targets/xiao-esp32c3-nostd");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("target.yaml"),
            "id: xiao-esp32c3-nostd\npins:\n  user_led: GPIO3   # LED 1\n  qwiic_i2c: { sda: GPIO6, scl: GPIO7 }\n",
        )
        .unwrap();
        assert_eq!(
            target_pins(dir.path(), "xiao-esp32c3-nostd").unwrap(),
            BTreeMap::from([("user_led".to_string(), "GPIO3".to_string())])
        );
        assert_eq!(
            target_pins(dir.path(), "nope").unwrap_err().code,
            "dev_error"
        );
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 10), "ab");
        assert_eq!(tail("aé", 1), "");
    }
}
