//! A run's events as its answer carries them: `events` in what `ter run
//! --json` prints and `ter serve` returns, for a page that plays the run
//! back (an LED on a board drawing, a logic analyzer, timed serial).
//!
//! They are the recording's own events, each as its line of `events.jsonl`
//! reads, in time order. A pin's event also says which pin of the board it
//! is, since a recording names pins as check.yaml does. Serial text is
//! without colour codes, as in `transcript_tail`. The list is bounded: a
//! long run is cut at a point in time, and the answer says where.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use ter_telemetry::Recording;
use ter_telemetry::event::EventKind;

/// The most events an answer carries.
pub const LIMIT: usize = 5000;
/// The most serial text an answer's events carry, in bytes.
pub const TEXT_LIMIT: usize = 64 * 1024;

/// The events of one run, ready for its answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub events: Vec<Value>,
    /// How long the capture ran after reset.
    pub end_us: u64,
    /// Set when the list is not the whole recording.
    pub cut: Option<Cut>,
}

/// Where a list was cut: every event before `from_us` is in it, and none
/// at or after `from_us` that the list does not hold already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub kept: usize,
    pub total: usize,
    pub from_us: u64,
}

/// `rec`'s events for an answer. `pins` is the target's pin map (a
/// check.yaml name to the board's own name for the pin, `GPIO3` or `PA5`);
/// `nets` is what a simulator recorded each pin as, when it has its own
/// names (`mcu.D1`).
pub fn shown(
    rec: &Recording,
    pins: &BTreeMap<String, String>,
    nets: &BTreeMap<String, String>,
) -> Shown {
    shown_within(rec, pins, nets, LIMIT, TEXT_LIMIT)
}

fn shown_within(
    rec: &Recording,
    pins: &BTreeMap<String, String>,
    nets: &BTreeMap<String, String>,
    limit: usize,
    text_limit: usize,
) -> Shown {
    let mut all: Vec<(u64, usize, Value)> = rec
        .events
        .iter()
        .filter_map(|event| {
            let mut value = serde_json::to_value(event).expect("event serialises");
            let mut text_len = 0;
            match &event.kind {
                EventKind::PinEdge { name, .. } => {
                    if let Some(pin) = pins.get(name) {
                        value["pin"] = json!(pin);
                    }
                    if let Some(net) = nets.get(name) {
                        value["net"] = json!(net);
                    }
                }
                EventKind::Serial { text, .. } => {
                    let plain = ter_telemetry::strip_ansi(text);
                    if plain.is_empty() {
                        return None;
                    }
                    text_len = plain.len();
                    value["text"] = json!(plain);
                }
                EventKind::Reset | EventKind::Power { .. } => {}
            }
            Some((event.t_us, text_len, value))
        })
        .collect();
    all.sort_by_key(|(t_us, ..)| *t_us);

    let total = all.len();
    let mut text = 0;
    let mut events = Vec::with_capacity(total.min(limit));
    let mut cut = None;
    for (t_us, text_len, value) in all {
        if events.len() == limit || text + text_len > text_limit {
            cut = Some(Cut {
                kept: events.len(),
                total,
                from_us: t_us,
            });
            break;
        }
        text += text_len;
        events.push(value);
    }
    Shown {
        events,
        end_us: rec.capture.end_us,
        cut,
    }
}

impl Shown {
    /// Put the events into a run's answer: `events`, `events_end_us`, and
    /// `events_cut` when the list was cut.
    pub fn put_into(self, answer: &mut Value) {
        answer["events"] = Value::Array(self.events);
        answer["events_end_us"] = json!(self.end_us);
        if let Some(cut) = self.cut {
            answer["events_cut"] =
                json!({"kept": cut.kept, "total": cut.total, "from_us": cut.from_us});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ter_telemetry::Capture;
    use ter_telemetry::event::{Event, Level};

    fn recording(events: Vec<Event>) -> Recording {
        Recording {
            capture: Capture {
                end_us: 5_500_000,
                ..Capture::default()
            },
            events,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn events_are_the_recordings_lines_with_the_boards_pin() {
        let rec = recording(vec![
            Event::reset(0),
            Event::pin(0, "user_led", Level::Low),
            Event::serial(412_000, "\u{1b}[32mtick\u{1b}[0m\n"),
            Event::pin(143_325, "user_led", Level::High),
            Event::pin(200_000, "spare", Level::High),
            Event::serial(500_000, "\u{1b}[0m"),
        ]);
        let shown = shown(
            &rec,
            &map(&[("user_led", "GPIO3")]),
            &map(&[("user_led", "mcu.D1")]),
        );
        assert_eq!(
            shown.events,
            vec![
                json!({"t_us": 0, "kind": "reset"}),
                json!({"t_us": 0, "kind": "pin_edge", "name": "user_led", "level": "low",
                       "pin": "GPIO3", "net": "mcu.D1"}),
                json!({"t_us": 143_325, "kind": "pin_edge", "name": "user_led", "level": "high",
                       "pin": "GPIO3", "net": "mcu.D1"}),
                // A pin neither map knows keeps its check.yaml name alone.
                json!({"t_us": 200_000, "kind": "pin_edge", "name": "spare", "level": "high"}),
                // In time order, without colour codes; a chunk that was
                // only colour codes is left out.
                json!({"t_us": 412_000, "kind": "serial", "text": "tick\n"}),
            ]
        );
        assert_eq!(shown.end_us, 5_500_000);
        assert_eq!(shown.cut, None);

        let mut answer = json!({"check_status": "passed"});
        shown.put_into(&mut answer);
        assert_eq!(answer["events"].as_array().unwrap().len(), 5);
        assert_eq!(answer["events_end_us"], 5_500_000);
        assert!(answer.get("events_cut").is_none(), "{answer}");
    }

    #[test]
    fn a_long_run_is_cut_at_a_time_and_says_so() {
        // A PWM pin: 20000 edges 50 us apart.
        let mut events = vec![Event::reset(0)];
        events.extend((0..20_000u64).map(|i| {
            let level = if i % 2 == 0 { Level::High } else { Level::Low };
            Event::pin(i * 50, "user_led", level)
        }));
        let rec = recording(events);
        let shown = shown(&rec, &map(&[]), &map(&[]));
        assert_eq!(shown.events.len(), LIMIT);
        assert_eq!(
            shown.cut,
            Some(Cut {
                kept: LIMIT,
                total: 20_001,
                // The first event left out: the 5000th edge (the reset is
                // the first event kept).
                from_us: 4_999 * 50,
            })
        );
        let mut answer = json!({});
        shown.put_into(&mut answer);
        assert_eq!(
            answer["events_cut"],
            json!({"kept": 5000, "total": 20_001, "from_us": 249_950})
        );
    }

    #[test]
    fn serial_text_is_bounded_too() {
        let rec = recording(vec![
            Event::serial(10, "0123456789"),
            Event::pin(15, "user_led", Level::High),
            Event::serial(20, "0123456789"),
            Event::serial(30, "x"),
        ]);
        let shown = shown_within(&rec, &map(&[]), &map(&[]), 100, 15);
        assert_eq!(shown.events.len(), 2, "{:?}", shown.events);
        assert_eq!(
            shown.cut,
            Some(Cut {
                kept: 2,
                total: 4,
                from_us: 20
            }),
            "nothing after the cut is kept, however small"
        );
    }
}
