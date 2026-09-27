//! Exercises as the site describes them.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
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
    /// The target's pin names and what each is on the chip (`user_led` =
    /// `GPIO3`): how a check's pin names reach the circuit. Bus groups
    /// (`qwiic_i2c: {sda, scl}`) are not pins a check can name and are left
    /// out.
    #[serde(default, deserialize_with = "pin_map")]
    pub pins: BTreeMap<String, String>,
}

/// The string-valued entries of a `pins:` map.
pub fn pin_map<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    let raw: Option<BTreeMap<String, Value>> = Option::deserialize(d)?;
    Ok(raw
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_keep_the_named_pins_and_drop_buses() {
        let ex: Exercise = serde_json::from_value(serde_json::json!({
            "exercise_id": "a--t", "target": "t", "files": [],
            "pins": {"user_led": "GPIO3", "qwiic_i2c": {"sda": "GPIO6"}}
        }))
        .unwrap();
        assert_eq!(
            ex.pins,
            BTreeMap::from([("user_led".to_string(), "GPIO3".to_string())])
        );
        let none: Exercise = serde_json::from_value(serde_json::json!({
            "exercise_id": "a--t", "target": "t", "files": [], "pins": null
        }))
        .unwrap();
        assert!(none.pins.is_empty());
    }
}
