//! Exercises as the site describes them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An exercise as `enrollments` lists it: enough to name and place it,
/// without its files.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ExerciseRef {
    pub exercise_id: String,
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub kind: String,
}

/// An exercise as `exercise` returns it: the scaffold a learner fetches.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Exercise {
    pub exercise_id: String,
    #[serde(default)]
    pub title: String,
    pub target: String,
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub runner_args: Value,
    #[serde(default)]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub toolchain: Value,
    #[serde(default)]
    pub instructions_md: String,
    pub files: Vec<ExerciseFile>,
    /// The ways this exercise may be run. `None` from a site that does not
    /// say, in which case the site's answer to `run` is the check.
    #[serde(default)]
    pub modes: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ExerciseFile {
    pub path: String,
    pub content: String,
    /// `utf8` or `base64`.
    #[serde(default = "utf8")]
    pub encoding: String,
}

fn utf8() -> String {
    "utf8".into()
}

/// Both modes, the site's order: what an exercise allows when nothing says
/// otherwise.
pub fn default_modes() -> Vec<String> {
    vec!["hardware".into(), "simulation".into()]
}
