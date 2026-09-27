//! A recorded run: `events.jsonl`, the file `ter-check` evaluates.
//!
//! The first line is the capture header (`{"capture": {...}}`): where the
//! run happened and what could be seen there. Every later line is one
//! [`Event`]. The header makes the file self-contained, so a recording is
//! re-checked with no venue, no account and no other file.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKinds, Stimulus};

pub const EVENTS_FILE: &str = "events.jsonl";

/// Where and how a recording was made.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capture {
    /// `wokwi`, `local`, ...
    pub venue: String,
    /// Instruments attached alongside the venue.
    #[serde(default)]
    pub instruments: Vec<String>,
    /// The target id, from ter.toml.
    pub target: String,
    /// The event kinds the venue and its instruments could see or drive.
    pub provides: EventKinds,
    /// The pins that were recorded. A pin check on any other pin was not
    /// seen, whatever `provides` says.
    #[serde(default)]
    pub pins: Vec<String>,
    /// The stimuli the venue applied, in order.
    #[serde(default)]
    pub stimuli: Vec<Stimulus>,
    /// How long the capture ran after reset.
    pub end_us: u64,
    pub cli_version: String,
    #[serde(default)]
    pub elf_sha256: String,
    /// RFC 3339 UTC.
    pub started_at: String,
    pub ended_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub capture: Capture,
    pub events: Vec<Event>,
}

#[derive(Serialize, Deserialize)]
struct Header {
    capture: Capture,
}

impl Recording {
    /// The whole `events.jsonl`.
    pub fn to_jsonl(&self) -> String {
        let mut out = serde_json::to_string(&Header {
            capture: self.capture.clone(),
        })
        .expect("capture serialises");
        out.push('\n');
        for e in &self.events {
            out.push_str(&serde_json::to_string(e).expect("event serialises"));
            out.push('\n');
        }
        out
    }

    pub fn from_jsonl(text: &str) -> Result<Self, String> {
        let mut lines = text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty());
        let (_, first) = lines.next().ok_or("the file is empty")?;
        let header: Header = serde_json::from_str(first)
            .map_err(|e| format!("line 1 is not a capture header: {e}"))?;
        let events = lines
            .map(|(i, l)| serde_json::from_str(l).map_err(|e| format!("line {}: {e}", i + 1)))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            capture: header.capture,
            events,
        })
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(self.to_jsonl().as_bytes())
    }

    /// `path` is an `events.jsonl` or a run folder holding one.
    pub fn read(path: &Path) -> Result<Self, String> {
        let file = if path.is_dir() {
            path.join(EVENTS_FILE)
        } else {
            path.to_path_buf()
        };
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("could not read {}: {e}", file.display()))?;
        Self::from_jsonl(&text).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// Everything the module printed, in order.
    pub fn serial_text(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match &e.kind {
                crate::event::EventKind::Serial { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Kind, Level, StimulusKind};

    #[test]
    fn a_recording_round_trips_through_jsonl() {
        let rec = Recording {
            capture: Capture {
                venue: "wokwi".into(),
                target: "xiao-esp32c3-nostd".into(),
                provides: [Kind::Serial, Kind::Pins, Kind::Reset, Kind::Stimulus].into(),
                pins: vec!["user_led".into()],
                stimuli: vec![Stimulus {
                    at_ms: 1000,
                    kind: StimulusKind::Press {
                        name: "user_button".into(),
                    },
                }],
                end_us: 4_000_000,
                cli_version: "0.1.0".into(),
                started_at: "2026-09-27T12:00:00Z".into(),
                ended_at: "2026-09-27T12:00:09Z".into(),
                ..Capture::default()
            },
            events: vec![
                Event::reset(0),
                Event::pin(0, "user_led", Level::Low),
                Event::serial(12, "boot\n"),
            ],
        };
        let text = rec.to_jsonl();
        assert!(text.starts_with(r#"{"capture":{"venue":"wokwi""#));
        assert_eq!(text.lines().count(), 4);
        assert_eq!(Recording::from_jsonl(&text).unwrap(), rec);
        assert_eq!(rec.serial_text(), "boot\n");
    }

    #[test]
    fn a_file_without_a_header_is_refused() {
        let err = Recording::from_jsonl(r#"{"t_us":0,"kind":"reset"}"#).unwrap_err();
        assert!(err.contains("capture header"), "{err}");
        assert!(Recording::from_jsonl("").is_err());
    }
}
