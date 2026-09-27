//! `check.toml`: the verdicts of one run, with where and what they judged.
//!
//! `ter run` writes it into the run folder; `ter telemetry check` on that
//! folder prints the same text, byte for byte, from the recording alone.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ter_telemetry::Capture;

use crate::CheckVerdict;

pub const REPORT_FILE: &str = "check.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub venue: String,
    pub instruments: Vec<String>,
    pub target: String,
    pub cli_version: String,
    pub elf_sha256: String,
    /// Of the check.yaml the verdicts come from.
    pub check_sha256: String,
    pub started_at: String,
    pub ended_at: String,
    #[serde(rename = "verdict", default)]
    pub verdicts: Vec<CheckVerdict>,
}

impl Report {
    pub fn new(capture: &Capture, check_yaml: &[u8], verdicts: Vec<CheckVerdict>) -> Self {
        Self {
            venue: capture.venue.clone(),
            instruments: capture.instruments.clone(),
            target: capture.target.clone(),
            cli_version: capture.cli_version.clone(),
            elf_sha256: capture.elf_sha256.clone(),
            check_sha256: Sha256::digest(check_yaml)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            started_at: capture.started_at.clone(),
            ended_at: capture.ended_at.clone(),
            verdicts,
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("report serialises")
    }
}
