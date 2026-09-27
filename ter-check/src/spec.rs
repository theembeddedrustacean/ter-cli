//! `check.yaml`: what a run must show.
//!
//! ```yaml
//! timeout_ms: 4000
//! setup:
//!   - press: user_button     # a pin name from target.yaml
//!     at_ms: 1000
//!     hold_ms: 1500
//! assert:
//!   - id: led-on-through-hold
//!     pin: user_led
//!     is: high
//!     between_ms: [1200, 2400]
//! ```
//!
//! Check kinds: `serial_contains` and `is` take one of `within_ms`,
//! `after_ms`, `between_ms`; `toggles_per_s`, `edge_count` and
//! `pulse_width_ms` take `{min, max}` (either or both) and `window_ms`.

use std::collections::BTreeSet;

use serde::Deserialize;
use ter_telemetry::{Kind, Level, Stimulus, StimulusKind};

/// A parsed, validated `check.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckFile {
    pub timeout_ms: u64,
    pub setup: Vec<Setup>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setup {
    /// Held for `hold_ms`, or to the end of the run.
    Press {
        pin: String,
        at_ms: u64,
        hold_ms: Option<u64>,
    },
    Release {
        pin: String,
        at_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub id: String,
    pub what: What,
}

#[derive(Debug, Clone, PartialEq)]
pub enum What {
    SerialContains {
        text: String,
        when: When,
    },
    Is {
        pin: String,
        level: Level,
        when: When,
    },
    TogglesPerS {
        pin: String,
        bounds: Bounds,
        window: Window,
    },
    EdgeCount {
        pin: String,
        bounds: Bounds,
        window: Window,
    },
    PulseWidthMs {
        pin: String,
        bounds: Bounds,
        window: Window,
    },
}

/// When a `serial_contains` or `is` check applies, in ms after reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Within(u64),
    After(u64),
    Between(u64, u64),
}

/// `window_ms: [start, end]`, in ms after reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(pub u64, pub u64);

/// Inclusive bounds; at least one is set.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Bounds {
    pub fn contains(&self, v: f64) -> bool {
        self.min.is_none_or(|m| v >= m) && self.max.is_none_or(|m| v <= m)
    }
}

impl Check {
    /// The pin the check watches, if it watches one.
    pub fn pin(&self) -> Option<&str> {
        match &self.what {
            What::SerialContains { .. } => None,
            What::Is { pin, .. }
            | What::TogglesPerS { pin, .. }
            | What::EdgeCount { pin, .. }
            | What::PulseWidthMs { pin, .. } => Some(pin),
        }
    }

    /// The kind name as `check.yaml` spells it.
    pub fn kind_name(&self) -> &'static str {
        match self.what {
            What::SerialContains { .. } => "serial_contains",
            What::Is { .. } => "is",
            What::TogglesPerS { .. } => "toggles_per_s",
            What::EdgeCount { .. } => "edge_count",
            What::PulseWidthMs { .. } => "pulse_width_ms",
        }
    }
}

impl CheckFile {
    pub fn parse(yaml: &str) -> Result<Self, String> {
        let raw: RawFile = serde_norway::from_str(yaml).map_err(|e| e.to_string())?;
        raw.validate()
    }

    /// What `check` needs to be seen: its own event kind, and stimulus
    /// when the file has a `setup:` (every check is judged under it).
    pub fn needs(&self, check: &Check) -> BTreeSet<Kind> {
        let mut kinds = BTreeSet::new();
        kinds.insert(match check.what {
            What::SerialContains { .. } => Kind::Serial,
            _ => Kind::Pins,
        });
        if !self.setup.is_empty() {
            kinds.insert(Kind::Stimulus);
        }
        kinds
    }

    /// The pins the checks watch, in check order, each once.
    pub fn watched_pins(&self) -> Vec<String> {
        let mut pins: Vec<String> = Vec::new();
        for pin in self.checks.iter().filter_map(Check::pin) {
            if !pins.iter().any(|p| p == pin) {
                pins.push(pin.to_string());
            }
        }
        pins
    }

    /// The pins the setup drives, each once.
    pub fn driven_pins(&self) -> Vec<String> {
        let mut pins: Vec<String> = Vec::new();
        for s in &self.setup {
            let (Setup::Press { pin, .. } | Setup::Release { pin, .. }) = s;
            if !pins.contains(pin) {
                pins.push(pin.clone());
            }
        }
        pins
    }

    /// The setup as timed stimuli: a press with `hold_ms` is a press and a
    /// release. In time order; at one instant, in file order.
    pub fn stimuli(&self) -> Vec<Stimulus> {
        let mut out = Vec::new();
        for s in &self.setup {
            match s {
                Setup::Press {
                    pin,
                    at_ms,
                    hold_ms,
                } => {
                    out.push(Stimulus {
                        at_ms: *at_ms,
                        kind: StimulusKind::Press { name: pin.clone() },
                    });
                    if let Some(hold) = hold_ms {
                        out.push(Stimulus {
                            at_ms: at_ms + hold,
                            kind: StimulusKind::Release { name: pin.clone() },
                        });
                    }
                }
                Setup::Release { pin, at_ms } => out.push(Stimulus {
                    at_ms: *at_ms,
                    kind: StimulusKind::Release { name: pin.clone() },
                }),
            }
        }
        out.sort_by_key(|s| s.at_ms);
        out
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    timeout_ms: u64,
    #[serde(default)]
    setup: Vec<RawSetup>,
    #[serde(rename = "assert", default)]
    asserts: Vec<RawCheck>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetup {
    press: Option<String>,
    release: Option<String>,
    at_ms: u64,
    hold_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    id: String,
    pin: Option<String>,
    serial_contains: Option<String>,
    is: Option<Level>,
    toggles_per_s: Option<Bounds>,
    edge_count: Option<Bounds>,
    pulse_width_ms: Option<Bounds>,
    within_ms: Option<u64>,
    after_ms: Option<u64>,
    between_ms: Option<[u64; 2]>,
    window_ms: Option<[u64; 2]>,
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

impl RawFile {
    fn validate(self) -> Result<CheckFile, String> {
        if self.timeout_ms == 0 {
            return Err("timeout_ms must be a positive number of milliseconds".into());
        }
        let setup = self
            .setup
            .into_iter()
            .enumerate()
            .map(|(i, s)| s.validate(i))
            .collect::<Result<Vec<_>, _>>()?;
        if self.asserts.is_empty() {
            return Err("the assert list is empty".into());
        }
        let mut ids = BTreeSet::new();
        let mut checks = Vec::new();
        for raw in self.asserts {
            if !is_slug(&raw.id) {
                return Err(format!("check id {:?} is not a slug", raw.id));
            }
            if !ids.insert(raw.id.clone()) {
                return Err(format!("check id {} repeats", raw.id));
            }
            checks.push(raw.validate()?);
        }
        Ok(CheckFile {
            timeout_ms: self.timeout_ms,
            setup,
            checks,
        })
    }
}

impl RawSetup {
    fn validate(self, i: usize) -> Result<Setup, String> {
        match (self.press, self.release) {
            (Some(pin), None) => {
                if self.hold_ms == Some(0) {
                    return Err(format!("setup[{i}]: hold_ms must be positive"));
                }
                Ok(Setup::Press {
                    pin,
                    at_ms: self.at_ms,
                    hold_ms: self.hold_ms,
                })
            }
            (None, Some(pin)) if self.hold_ms.is_none() => Ok(Setup::Release {
                pin,
                at_ms: self.at_ms,
            }),
            (None, Some(_)) => Err(format!("setup[{i}]: hold_ms is for press only")),
            _ => Err(format!("setup[{i}] needs exactly one of press, release")),
        }
    }
}

impl RawCheck {
    fn validate(self) -> Result<Check, String> {
        let id = self.id;
        let err = |m: String| format!("check {id}: {m}");
        let kinds = [
            self.serial_contains.is_some(),
            self.is.is_some(),
            self.toggles_per_s.is_some(),
            self.edge_count.is_some(),
            self.pulse_width_ms.is_some(),
        ];
        if kinds.iter().filter(|k| **k).count() != 1 {
            return Err(err(
                "needs exactly one of serial_contains, is, toggles_per_s, edge_count, pulse_width_ms"
                    .into(),
            ));
        }

        let when = || -> Result<When, String> {
            let set = [
                self.within_ms.map(When::Within),
                self.after_ms.map(When::After),
                self.between_ms.map(|[a, b]| When::Between(a, b)),
            ];
            let set: Vec<When> = set.into_iter().flatten().collect();
            if set.len() != 1 || self.window_ms.is_some() {
                return Err(err(
                    "needs exactly one of within_ms, after_ms, between_ms".into()
                ));
            }
            if let When::Between(a, b) = set[0]
                && a > b
            {
                return Err(err("between_ms must be [start, end]".into()));
            }
            Ok(set[0])
        };
        let window = || -> Result<Window, String> {
            if self.within_ms.is_some() || self.after_ms.is_some() || self.between_ms.is_some() {
                return Err(err(
                    "takes window_ms, not within_ms, after_ms or between_ms".into(),
                ));
            }
            match self.window_ms {
                Some([a, b]) if a < b => Ok(Window(a, b)),
                Some(_) => Err(err("window_ms must be [start, end] with start < end".into())),
                None => Err(err("needs window_ms".into())),
            }
        };
        let pin = || -> Result<String, String> {
            self.pin.clone().ok_or_else(|| err("needs a pin".into()))
        };
        let bounds = |b: Bounds| -> Result<Bounds, String> {
            let ok = |v: Option<f64>| v.is_none_or(|v| v.is_finite() && v >= 0.0);
            if (b.min.is_none() && b.max.is_none()) || !ok(b.min) || !ok(b.max) {
                return Err(err("bounds must be {min, max}, non-negative".into()));
            }
            if let (Some(lo), Some(hi)) = (b.min, b.max)
                && lo > hi
            {
                return Err(err("min is above max".into()));
            }
            Ok(b)
        };

        let what = if let Some(text) = self.serial_contains.clone() {
            if self.pin.is_some() {
                return Err(err("serial_contains takes no pin".into()));
            }
            if text.trim().is_empty() {
                return Err(err("serial_contains must not be empty".into()));
            }
            What::SerialContains {
                text,
                when: when()?,
            }
        } else if let Some(level) = self.is {
            What::Is {
                pin: pin()?,
                level,
                when: when()?,
            }
        } else if let Some(b) = self.toggles_per_s {
            What::TogglesPerS {
                pin: pin()?,
                bounds: bounds(b)?,
                window: window()?,
            }
        } else if let Some(b) = self.edge_count {
            What::EdgeCount {
                pin: pin()?,
                bounds: bounds(b)?,
                window: window()?,
            }
        } else {
            What::PulseWidthMs {
                pin: pin()?,
                bounds: bounds(self.pulse_width_ms.expect("one kind is set"))?,
                window: window()?,
            }
        };
        Ok(Check { id, what })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUTTON_BLINK: &str = "\
timeout_ms: 4000
setup:
  - press: user_button
    at_ms: 1000
    hold_ms: 1500
assert:
  - id: led-off-at-start
    pin: user_led
    is: low
    between_ms: [600, 950]
  - id: led-toggles-once-on-press
    pin: user_led
    edge_count: { min: 1, max: 1 }
    window_ms: [950, 1150]
  - id: banner
    serial_contains: \"ready\"
    within_ms: 2000
";

    #[test]
    fn a_curriculum_check_parses() {
        let f = CheckFile::parse(BUTTON_BLINK).unwrap();
        assert_eq!(f.timeout_ms, 4000);
        assert_eq!(f.checks.len(), 3);
        assert_eq!(
            f.checks[0].what,
            What::Is {
                pin: "user_led".into(),
                level: Level::Low,
                when: When::Between(600, 950)
            }
        );
        assert_eq!(
            f.checks[1].what,
            What::EdgeCount {
                pin: "user_led".into(),
                bounds: Bounds {
                    min: Some(1.0),
                    max: Some(1.0)
                },
                window: Window(950, 1150)
            }
        );
        assert_eq!(f.watched_pins(), vec!["user_led".to_string()]);
        assert_eq!(f.driven_pins(), vec!["user_button".to_string()]);
    }

    #[test]
    fn a_press_with_hold_is_scheduled_as_press_then_release() {
        let f = CheckFile::parse(BUTTON_BLINK).unwrap();
        let press = |at_ms| Stimulus {
            at_ms,
            kind: StimulusKind::Press {
                name: "user_button".into(),
            },
        };
        let release = |at_ms| Stimulus {
            at_ms,
            kind: StimulusKind::Release {
                name: "user_button".into(),
            },
        };
        assert_eq!(f.stimuli(), vec![press(1000), release(2500)]);

        let two = "timeout_ms: 9000\nsetup:\n  - press: b\n    at_ms: 3000\n  - release: b\n    at_ms: 5000\n  - press: b\n    at_ms: 1000\n    hold_ms: 200\nassert:\n  - id: x\n    serial_contains: y\n    within_ms: 1\n";
        let f = CheckFile::parse(two).unwrap();
        let t: Vec<u64> = f.stimuli().iter().map(|s| s.at_ms).collect();
        assert_eq!(t, vec![1000, 1200, 3000, 5000], "time order");
    }

    #[test]
    fn needs_follow_the_kind_and_the_setup() {
        let f = CheckFile::parse(BUTTON_BLINK).unwrap();
        assert_eq!(f.needs(&f.checks[0]), [Kind::Pins, Kind::Stimulus].into());
        assert_eq!(f.needs(&f.checks[2]), [Kind::Serial, Kind::Stimulus].into());
        let plain = CheckFile::parse(
            "timeout_ms: 1\nassert:\n  - id: a\n    serial_contains: x\n    after_ms: 0\n",
        )
        .unwrap();
        assert_eq!(plain.needs(&plain.checks[0]), [Kind::Serial].into());
    }

    #[test]
    fn authoring_mistakes_are_refused() {
        let bad = |body: &str| {
            CheckFile::parse(&format!("timeout_ms: 5000\nassert:\n{body}")).unwrap_err()
        };
        assert!(bad("  - id: A\n    serial_contains: x\n    within_ms: 1\n").contains("slug"));
        assert!(bad("  - id: a\n    pin: p\n    is: high\n").contains("exactly one of within"));
        assert!(
            bad("  - id: a\n    pin: p\n    edge_count: {min: 1}\n").contains("needs window_ms")
        );
        assert!(
            bad("  - id: a\n    pin: p\n    edge_count: {}\n    window_ms: [0, 1]\n")
                .contains("bounds")
        );
        assert!(bad("  - id: a\n    is: high\n    after_ms: 1\n").contains("needs a pin"));
        assert!(bad("  - id: a\n    pin: p\n    is: middle\n    after_ms: 1\n").contains("middle"));
        assert!(
            bad("  - id: a\n    serial_contains: x\n    within_ms: 1\n  - id: a\n    serial_contains: y\n    within_ms: 1\n")
                .contains("repeats")
        );
        assert!(
            bad("  - id: a\n    pin: p\n    toggles_per_s: {min: 1}\n    window_ms: [5, 5]\n")
                .contains("start < end")
        );
        assert!(
            bad("  - id: a\n    serial_contains: x\n    within_ms: 1\n    colour: red\n")
                .contains("colour")
        );
        assert!(CheckFile::parse("timeout_ms: 5\nassert: []\n").is_err());
        assert!(
            CheckFile::parse("timeout_ms: 5\nsetup:\n  - release: b\n    at_ms: 1\n    hold_ms: 2\nassert:\n  - id: a\n    serial_contains: x\n    within_ms: 1\n")
                .unwrap_err()
                .contains("press only")
        );
    }
}
