//! The event stream a run records, and the stimuli that drive it.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A logic level on a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Low,
    High,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Low => "low",
            Level::High => "high",
        })
    }
}

/// One thing that happened, `t_us` microseconds after the `Reset` that
/// starts every recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub t_us: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    Reset,
    /// The level of a named pin (a name from the target's `pins:`, never a
    /// GPIO number). The first event of a pin is its level when the capture
    /// started; each later one is a change.
    PinEdge {
        name: String,
        level: Level,
    },
    /// Bytes from the module's UART, as text. `span_us` is set when the
    /// venue only knows the bytes appeared somewhere in
    /// `[t_us, t_us + span_us)`, not the exact instant.
    Serial {
        text: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        span_us: u64,
    },
    Power {
        on: bool,
    },
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl Event {
    pub fn reset(t_us: u64) -> Self {
        Self {
            t_us,
            kind: EventKind::Reset,
        }
    }

    pub fn pin(t_us: u64, name: &str, level: Level) -> Self {
        Self {
            t_us,
            kind: EventKind::PinEdge {
                name: name.into(),
                level,
            },
        }
    }

    pub fn serial(t_us: u64, text: &str) -> Self {
        Self {
            t_us,
            kind: EventKind::Serial {
                text: text.into(),
                span_us: 0,
            },
        }
    }
}

/// Something a venue does to the circuit at a set time after reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stimulus {
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: StimulusKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StimulusKind {
    Press { name: String },
    Release { name: String },
    Reset,
    Power { on: bool },
}

/// What a venue or instrument can see or drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Serial,
    Pins,
    Power,
    Reset,
    /// Pressing and releasing named lines on a schedule.
    Stimulus,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Serial => "serial",
            Kind::Pins => "pins",
            Kind::Power => "power",
            Kind::Reset => "reset",
            Kind::Stimulus => "stimulus",
        })
    }
}

pub type EventKinds = BTreeSet<Kind>;

/// Put the origin of `events` on their first `Reset`, dropping what came
/// before it. With no `Reset`, the venue could not see one (a board the
/// flasher reset, say): one is synthesised at the start of the capture,
/// which is `t_us` 0. After this, `t_us` means the same on every venue.
pub fn from_reset(mut events: Vec<Event>) -> Vec<Event> {
    events.sort_by_key(|e| e.t_us);
    match events.iter().position(|e| e.kind == EventKind::Reset) {
        Some(i) => {
            let origin = events[i].t_us;
            events
                .drain(i..)
                .map(|mut e| {
                    e.t_us -= origin;
                    e
                })
                .collect()
        }
        None => {
            events.insert(0, Event::reset(0));
            events
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialise_flat_with_a_kind_tag() {
        let e = Event::pin(1500, "user_led", Level::High);
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t_us":1500,"kind":"pin_edge","name":"user_led","level":"high"}"#
        );
        let s = Event::serial(10, "hi\n");
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"t_us":10,"kind":"serial","text":"hi\n"}"#
        );
        let back: Event = serde_json::from_str(r#"{"t_us":0,"kind":"reset"}"#).unwrap();
        assert_eq!(back, Event::reset(0));
    }

    #[test]
    fn origin_moves_to_the_reset_event() {
        let events = vec![
            Event::serial(100, "boot junk"),
            Event::reset(1_000),
            Event::pin(1_500, "user_led", Level::High),
        ];
        assert_eq!(
            from_reset(events),
            vec![Event::reset(0), Event::pin(500, "user_led", Level::High)]
        );
    }

    #[test]
    fn a_reset_is_synthesised_at_capture_start_when_the_venue_emits_none() {
        let events = vec![
            Event::pin(2_000, "user_led", Level::Low),
            Event::serial(2_500, "x"),
        ];
        assert_eq!(
            from_reset(events),
            vec![
                Event::reset(0),
                Event::pin(2_000, "user_led", Level::Low),
                Event::serial(2_500, "x")
            ]
        );
    }
}
