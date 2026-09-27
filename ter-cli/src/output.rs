//! How `ter` reports results and failures, as text or as `--json`.

use serde::Serialize;
use serde_json::json;

/// A failure with the stable code `ter` prints and exits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: String,
    pub message: String,
    /// The command already printed its `--json` answer, with this error in
    /// it; only the exit status is left to do.
    pub in_json_answer: bool,
}

impl CliError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            in_json_answer: false,
        }
    }
}

impl From<ter_sdk::Error> for CliError {
    fn from(e: ter_sdk::Error) -> Self {
        Self::new(e.code(), e.to_string())
    }
}

impl From<ter_sdk::project::ProjectError> for CliError {
    fn from(e: ter_sdk::project::ProjectError) -> Self {
        Self::new(e.code, e.message)
    }
}

/// With `--json` the error goes to stdout as `{"error": {code, message}}`,
/// the same envelope the site uses; otherwise to stderr.
pub fn print_error(err: &CliError, json: bool) {
    if json && err.in_json_answer {
        return;
    }
    if json {
        println!(
            "{}",
            json!({"error": {"code": err.code, "message": err.message}})
        );
    } else {
        eprintln!("error[{}]: {}", err.code, err.message);
    }
}

pub fn print_json<T: Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("output serialises")
    );
}
