//! How `ter` reports results and failures, as text or as `--json`.

use serde::Serialize;
use serde_json::json;

/// A failure with the stable code `ter` prints and exits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: String,
    pub message: String,
}

impl CliError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<ter_sdk::Error> for CliError {
    fn from(e: ter_sdk::Error) -> Self {
        Self::new(e.code(), e.to_string())
    }
}

/// With `--json` the error goes to stdout as `{"error": {code, message}}`,
/// the same envelope the site uses; otherwise to stderr.
pub fn print_error(err: &CliError, json: bool) {
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
