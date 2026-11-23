//! The sim86 venue: the exercise's board in the sim86 cloud simulator, on
//! the learner's own sim86 account.
//!
//! sim86 is driven by its Python package. Its command prints how many
//! edges a net had, not when, so ter ships a small helper script
//! ([`HELPER`]) that runs beside the package and prints one JSON object:
//! the recording, or an error with a `kind`. ter never reads sim86's
//! words to decide anything.
//!
//! Board: `sim86/board.json` in the exercise folder (parts and wires).
//! `sim86/pins.json` maps the names `check.yaml` uses to it: a watched pin
//! to a net (`"user_led": "mcu.D1"`), a pressed one to a button part
//! (`{"part": "btn1"}`) or a board input (`{"part": "mcu", "input":
//! "button"}`). sim86 records nets, so a watched pin needs a wire to
//! another part.
//!
//! Time: the run starts paused at 0 ns, the nets are watched there, and
//! only then does the board run, so the recording holds the first edge. A
//! run that did not start paused at 0 is never judged.
//!
//! Every edge: a net's trace comes back exact only while it holds few
//! enough edges, and as a summary past that. The helper reads a busy net
//! in slices small enough to be exact and checks that they join; a net it
//! cannot read in full fails the run as a venue failure, never a pass.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::event::{Event, EventKind, EventKinds, Kind, Level, Stimulus, StimulusKind};
use crate::recording::{Capture, Recording};
use crate::venue::{Recovery, Venue, VenueError};
use crate::wokwi::{Ended, run_with_budget};

/// The board, in an exercise folder.
pub const BOARD_FILE: &str = "sim86/board.json";
/// `check.yaml`'s names on that board, in an exercise folder.
pub const PINS_FILE: &str = "sim86/pins.json";
/// The helper script, as `ter install sim86` writes it.
pub const HELPER: &str = include_str!("sim86_helper.py");
pub const HELPER_FILE: &str = "ter_sim86.py";
/// The `helper` number in the script's answers.
pub const HELPER_VERSION: u64 = 2;
/// What `pip install` is asked for.
pub const PACKAGE: &str = "sim86>=0.1.4,<0.2";
/// The package version this ter was tested with.
pub const TESTED: &str = "0.1.4";
/// The learner's sim86 key, when it is in the environment.
pub const KEY_ENV: &str = "SIM86_API_KEY";

pub const REQUEST_FILE: &str = "sim86-request.json";
/// What the helper printed: its answer, and anything on stderr.
pub const OUTPUT_LOG: &str = "sim86.json";
pub const ERROR_LOG: &str = "sim86.err";
/// The merged flash image of an ESP program, for the length of the run.
pub const FIRMWARE: &str = "firmware.bin";

/// How much simulated time a run is given past the check's timeout.
const TAIL_MS: u64 = 300;
/// Serial lines are stamped in whole simulated milliseconds.
const SERIAL_SPAN_US: u64 = 1_000;

/// Why a sim86 board cannot show what a check needs. Found before
/// anything builds; each names what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WiringError {
    BadBoard(String),
    BadPins(String),
    /// No part of the board takes a program, or more than one does.
    Programs(usize),
    /// `pins.json` does not name this pin.
    NotInPins(String),
    /// A watched pin must map to a net, `"<part>.<pin>"`.
    NotANet(String),
    /// A pressed pin must map to a button part or a board input.
    NotAButton(String),
    /// The net has no wire to another part, so sim86 records nothing.
    NoWire {
        pin: String,
        net: String,
    },
    /// The part a pin names is not on the board.
    NoPart {
        pin: String,
        part: String,
    },
}

impl std::fmt::Display for WiringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WiringError::BadBoard(e) => write!(f, "{BOARD_FILE} is not a sim86 board: {e}"),
            WiringError::BadPins(e) => write!(f, "{PINS_FILE} is not a pin map: {e}"),
            WiringError::Programs(0) => write!(f, "{BOARD_FILE} has no part that takes a program"),
            WiringError::Programs(n) => write!(
                f,
                "{BOARD_FILE} has {n} parts that take a program, and ter runs one"
            ),
            WiringError::NotInPins(p) => write!(f, "{PINS_FILE} has no pin named {p}"),
            WiringError::NotANet(p) => write!(
                f,
                "{PINS_FILE} must map {p} to a net (\"<part>.<pin>\") for a check to watch it"
            ),
            WiringError::NotAButton(p) => write!(
                f,
                "{PINS_FILE} must map {p} to a button ({{\"part\": ...}}) or a board input ({{\"part\": ..., \"input\": ...}}) for the setup to press it"
            ),
            WiringError::NoWire { pin, net } => write!(
                f,
                "{pin} is {net}, which has no wire to another part in {BOARD_FILE}, so sim86 records nothing on it"
            ),
            WiringError::NoPart { pin, part } => {
                write!(
                    f,
                    "{pin} names the part {part}, which is not in {BOARD_FILE}"
                )
            }
        }
    }
}

impl WiringError {
    /// The pin the problem is on, when it is on one.
    pub fn pin(&self) -> Option<&str> {
        match self {
            WiringError::NotInPins(pin)
            | WiringError::NotANet(pin)
            | WiringError::NotAButton(pin)
            | WiringError::NoWire { pin, .. }
            | WiringError::NoPart { pin, .. } => Some(pin),
            WiringError::BadBoard(_) | WiringError::BadPins(_) | WiringError::Programs(_) => None,
        }
    }
}

/// A watched pin and the net sim86 records for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub pin: String,
    pub net: String,
}

/// What a `setup:` press drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Button {
    /// A button part: `pressed` and `released`.
    Part { part: String },
    /// A board's own input, set to 1 and 0.
    Input { part: String, input: String },
}

/// What ter asks of the board for one check file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The id of the part that takes the program.
    pub mcu: String,
    /// One per distinct watched pin, in check order.
    pub watch: Vec<Watch>,
    pub buttons: BTreeMap<String, Button>,
}

/// One end of a wire, as `(part, pin)`.
fn wire_end(v: &Value) -> Option<(String, String)> {
    let (part, pin) = v.as_str()?.split_once('.')?;
    Some((part.to_string(), pin.to_string()))
}

/// A board file's wires, in any of the forms sim86 reads.
fn wires(board: &Value) -> Result<Vec<[(String, String); 2]>, WiringError> {
    let list = board
        .get("wires")
        .or_else(|| board.get("connections"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    list.iter()
        .enumerate()
        .map(|(i, w)| {
            let ends = if let Some(pair) = w.as_array().filter(|p| p.len() == 2) {
                wire_end(&pair[0]).zip(wire_end(&pair[1]))
            } else if w.get("from").is_some() {
                wire_end(&w["from"]).zip(wire_end(&w["to"]))
            } else {
                let field = |k: &str| w.get(k).and_then(Value::as_str).map(str::to_string);
                field("source")
                    .zip(field("sourceHandle"))
                    .zip(field("target").zip(field("targetHandle")))
            };
            ends.map(|(a, b)| [a, b])
                .ok_or_else(|| WiringError::BadBoard(format!("wire {i} does not name two pins")))
        })
        .collect()
}

/// Find the nets and buttons for a check file. `watch` are the pins the
/// checks name, in check order (repeats allowed); `press` the pins the
/// setup drives.
pub fn plan(
    board_json: &str,
    pins_json: &str,
    watch: &[String],
    press: &[String],
) -> Result<Plan, WiringError> {
    let board: Value =
        serde_json::from_str(board_json).map_err(|e| WiringError::BadBoard(e.to_string()))?;
    let parts = board
        .get("parts")
        .or_else(|| board.get("components"))
        .and_then(Value::as_array)
        .ok_or_else(|| WiringError::BadBoard("no parts list".into()))?;
    let ids: Vec<&str> = parts.iter().filter_map(|p| p["id"].as_str()).collect();
    if ids.len() != parts.len() {
        return Err(WiringError::BadBoard("a part has no id".into()));
    }
    let programmed: Vec<&str> = parts
        .iter()
        .filter(|p| p.get("firmware").is_some_and(|f| !f.is_null()))
        .filter_map(|p| p["id"].as_str())
        .collect();
    let [mcu] = programmed[..] else {
        return Err(WiringError::Programs(programmed.len()));
    };
    let wires = wires(&board)?;

    let pins: Value =
        serde_json::from_str(pins_json).map_err(|e| WiringError::BadPins(e.to_string()))?;
    let pins = pins
        .as_object()
        .ok_or_else(|| WiringError::BadPins("not a JSON object".into()))?;
    let on_board = |pin: &str, part: &str| {
        ids.contains(&part)
            .then_some(())
            .ok_or_else(|| WiringError::NoPart {
                pin: pin.to_string(),
                part: part.to_string(),
            })
    };

    let mut watched: Vec<Watch> = Vec::new();
    for pin in watch {
        if watched.iter().any(|w| w.pin == *pin) {
            continue;
        }
        let entry = pins
            .get(pin)
            .ok_or_else(|| WiringError::NotInPins(pin.clone()))?;
        let (net, end) = entry
            .as_str()
            .and_then(|net| Some((net, wire_end(&json!(net))?)))
            .ok_or_else(|| WiringError::NotANet(pin.clone()))?;
        on_board(pin, &end.0)?;
        // The net is recorded only if a wire takes it to another part.
        let wired = wires.iter().any(|[a, b]| {
            (*a == end && b.0 != end.0 && ids.contains(&b.0.as_str()))
                || (*b == end && a.0 != end.0 && ids.contains(&a.0.as_str()))
        });
        if !wired {
            return Err(WiringError::NoWire {
                pin: pin.clone(),
                net: net.to_string(),
            });
        }
        watched.push(Watch {
            pin: pin.clone(),
            net: net.to_string(),
        });
    }

    let mut buttons = BTreeMap::new();
    for pin in press {
        let entry = pins
            .get(pin)
            .ok_or_else(|| WiringError::NotInPins(pin.clone()))?;
        let part = entry
            .get("part")
            .and_then(Value::as_str)
            .ok_or_else(|| WiringError::NotAButton(pin.clone()))?;
        on_board(pin, part)?;
        let button = match entry.get("input").and_then(Value::as_str) {
            Some(input) => Button::Input {
                part: part.to_string(),
                input: input.to_string(),
            },
            None => Button::Part {
                part: part.to_string(),
            },
        };
        buttons.insert(pin.clone(), button);
    }
    Ok(Plan {
        mcu: mcu.to_string(),
        watch: watched,
        buttons,
    })
}

/// The presses and releases of `stimuli` as a sim86 scenario, planned
/// before the board starts. `None` when there is nothing to press.
pub fn scenario(stimuli: &[Stimulus], buttons: &BTreeMap<String, Button>) -> Option<Value> {
    let events: Vec<Value> = stimuli
        .iter()
        .filter_map(|s| {
            let (name, down) = match &s.kind {
                StimulusKind::Press { name } => (name, true),
                StimulusKind::Release { name } => (name, false),
                // sim86 runs have no power stimulus and one reset, at 0.
                StimulusKind::Reset | StimulusKind::Power { .. } => return None,
            };
            let env = match buttons.get(name)? {
                Button::Part { part } => json!({
                    "part": part,
                    "action": if down { "pressed" } else { "released" },
                }),
                Button::Input { part, input } => json!({
                    "part": part,
                    "input": input,
                    "value": if down { 1.0 } else { 0.0 },
                }),
            };
            Some(json!({"at": format!("{}ms", s.at_ms), "env": env}))
        })
        .collect();
    (!events.is_empty()).then(|| json!({"name": "ter-setup", "events": events}))
}

/// What a helper call came to when it did not give what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// sim86's own words, or the helper's.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// No sim86 key on this machine.
    NoKey,
    /// sim86 refused the key.
    Auth,
    /// The account has no credits left.
    Credits,
    /// sim86 does not take the board file.
    Board,
    /// sim86 does not take the program.
    Image,
    /// No simulator came free.
    Busy,
    /// sim86 wants a newer package.
    Version,
    /// sim86 refused to start the run.
    Refused,
    /// The run ended before the check's time was up.
    Ended,
    /// The run did not start paused at 0, so its start was not recorded.
    NotPaused,
    /// A net's edges could not be read in full.
    Incomplete,
    /// A press or release was not applied at its time.
    NotApplied,
    /// The sim86 package is not in the Python ter uses.
    Missing,
    /// Any other error from sim86.
    Api,
    /// The helper itself failed, or printed no answer.
    Helper,
}

impl FailureKind {
    fn named(kind: &str) -> Self {
        match kind {
            "no_key" => Self::NoKey,
            "auth" => Self::Auth,
            "credits" => Self::Credits,
            "board" => Self::Board,
            "image" => Self::Image,
            "busy" => Self::Busy,
            "version" => Self::Version,
            "refused" => Self::Refused,
            "ended" => Self::Ended,
            "not_paused" => Self::NotPaused,
            "incomplete" => Self::Incomplete,
            "missing" => Self::Missing,
            "api" => Self::Api,
            _ => Self::Helper,
        }
    }
}

impl Failure {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// One line: what happened and what to do about it.
    pub fn explain(&self) -> String {
        let said = self.message.trim();
        let said = said.lines().next().unwrap_or_default();
        match self.kind {
            FailureKind::NoKey => format!(
                "No sim86 key on this machine. sim86 runs on your own sim86 account: sign in with `sim86 login` (ter install sim86 says where it is), or set {KEY_ENV}"
            ),
            FailureKind::Auth => format!(
                "sim86 refused your key: it is wrong or revoked. Sign in again with `sim86 login`, or put a new key in {KEY_ENV}"
            ),
            FailureKind::Credits => format!(
                "Your sim86 account has no credits left for a run ({said}). Add credits on your sim86 account, then run again"
            ),
            FailureKind::Board => format!(
                "sim86 does not take this exercise's board ({said}). {BOARD_FILE} needs fixing in the curriculum"
            ),
            FailureKind::Image => format!("sim86 does not take the program as built ({said})"),
            FailureKind::Busy => {
                "Every sim86 simulator is busy. Try again in a few minutes".to_string()
            }
            FailureKind::Version => format!(
                "sim86 no longer takes the sim86 package installed here ({said}). Update it with `ter install sim86`; if that does not help, update ter"
            ),
            FailureKind::Refused => format!("sim86 refused to start the run ({said})"),
            FailureKind::Ended => {
                format!("The sim86 run ended before the check's time was up ({said})")
            }
            FailureKind::NotPaused => format!(
                "sim86 did not hold the board at its start, so the first moments were not recorded and the run is not judged ({said})"
            ),
            FailureKind::Incomplete => format!(
                "sim86 could not give every edge of a watched pin, so the run is not judged ({said})"
            ),
            FailureKind::NotApplied => format!(
                "sim86 did not apply the setup's input at its time, so the run is not judged ({said})"
            ),
            FailureKind::Missing => format!(
                "The sim86 package is not installed where ter looks for it ({said}). Run `ter install sim86`"
            ),
            FailureKind::Api => format!("sim86 failed ({said})"),
            FailureKind::Helper => format!("ter's sim86 helper failed ({said})"),
        }
    }
}

/// One watched net, as the helper read it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Net {
    /// The level at 0 ns: 0, 1, or unknown.
    pub initial: Option<u8>,
    /// `[time in ns, new level]`, in time order.
    pub edges: Vec<(u64, u8)>,
}

/// A scenario event sim86 reported.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Applied {
    #[serde(default, rename = "atNs")]
    pub at_ns: Option<u64>,
    #[serde(default)]
    pub event: Option<String>,
}

/// The account, from the helper's `balance`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Balance {
    /// One credit is one simulated minute.
    pub credits: i64,
    #[serde(default)]
    pub plan: String,
}

#[derive(Debug, Deserialize)]
struct RawError {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    message: String,
}

/// The helper's answer to any call.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub helper: u64,
    /// The sim86 package's version.
    #[serde(default)]
    pub sim86: Option<String>,
    #[serde(default)]
    pub python: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub started_ns: Option<u64>,
    #[serde(default)]
    pub paused_at_start: Option<bool>,
    #[serde(default)]
    pub end_ns: Option<u64>,
    #[serde(default)]
    pub pins: BTreeMap<String, Net>,
    /// `[simulated ms, source, line]`.
    #[serde(default)]
    pub serial: Vec<(f64, String, String)>,
    #[serde(default)]
    pub applied: Vec<Applied>,
    #[serde(default)]
    pub balance: Option<Balance>,
    /// Whether the package finds a key, from the helper's `version`.
    #[serde(default)]
    pub key: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Raw {
    ok: bool,
    #[serde(default)]
    error: Option<RawError>,
    #[serde(flatten)]
    answer: Answer,
}

/// Read what the helper printed: its last line that is a JSON object.
pub fn parse(stdout: &str) -> Result<Answer, Failure> {
    let raw: Raw = stdout
        .lines()
        .rev()
        .filter(|l| l.trim_start().starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| Failure::new(FailureKind::Helper, "it printed no answer"))?;
    if raw.ok {
        return Ok(raw.answer);
    }
    let error = raw.error.unwrap_or(RawError {
        kind: String::new(),
        message: "it gave no reason".into(),
    });
    Err(Failure::new(FailureKind::named(&error.kind), error.message))
}

/// The events of a run the helper recorded, or why it is not judged.
/// `stimuli` are the ones ter planned; each must have been applied on time.
pub fn events(
    answer: &Answer,
    plan: &Plan,
    stimuli: &[Stimulus],
    timeout_ms: u64,
) -> Result<Vec<Event>, Failure> {
    let fail = |kind, message: String| Err(Failure::new(kind, message));
    if answer.paused_at_start != Some(true) || answer.started_ns != Some(0) {
        return fail(
            FailureKind::NotPaused,
            match answer.started_ns {
                Some(ns) => format!("the recording starts {:.3} ms in", ns as f64 / 1e6),
                None => "it did not say where the recording starts".into(),
            },
        );
    }
    let end_ns = timeout_ms * 1_000_000;
    if answer.end_ns != Some(end_ns) {
        return fail(
            FailureKind::Ended,
            format!("the recording does not reach {timeout_ms} ms"),
        );
    }
    let pressed = |s: &Stimulus| match &s.kind {
        StimulusKind::Press { name } | StimulusKind::Release { name } => Some(name.clone()),
        StimulusKind::Reset | StimulusKind::Power { .. } => None,
    };
    for s in stimuli.iter().filter(|s| s.at_ms < timeout_ms) {
        let Some(name) = pressed(s) else { continue };
        let wanted = stimuli
            .iter()
            .filter(|x| x.at_ms == s.at_ms && pressed(x).is_some())
            .count();
        let acted = answer
            .applied
            .iter()
            .filter(|a| a.at_ns == Some(s.at_ms * 1_000_000) && a.event.as_deref() == Some("acted"))
            .count();
        if !plan.buttons.contains_key(&name) || acted < wanted {
            return fail(FailureKind::NotApplied, format!("{name} at {} ms", s.at_ms));
        }
    }

    let mut out = vec![Event::reset(0)];
    for watch in &plan.watch {
        let Some(net) = answer.pins.get(&watch.net) else {
            return fail(
                FailureKind::Incomplete,
                format!("{}: no recording came back", watch.net),
            );
        };
        let level = |v: u8| if v == 0 { Level::Low } else { Level::High };
        let mut last = net.initial.map(level);
        if let Some(initial) = last {
            out.push(Event::pin(0, &watch.pin, initial));
        }
        for &(at_ns, value) in &net.edges {
            if at_ns > end_ns || last == Some(level(value)) {
                continue;
            }
            last = Some(level(value));
            out.push(Event::pin((at_ns + 500) / 1_000, &watch.pin, level(value)));
        }
    }
    // A board can print the same line on two ports at once (the boot
    // messages of an ESP on its UART and its USB serial): once is enough.
    let prefix = format!("{}:", plan.mcu);
    let mut seen: Vec<(u64, &str, &str)> = Vec::new();
    for (t_ms, source, line) in &answer.serial {
        if source != &plan.mcu && !source.starts_with(&prefix) {
            continue;
        }
        let t_us = (t_ms.max(0.0) * 1_000.0) as u64;
        if t_us > end_ns / 1_000 {
            continue;
        }
        if seen
            .iter()
            .any(|(t, s, l)| *t == t_us && *l == line.as_str() && *s != source.as_str())
        {
            continue;
        }
        seen.push((t_us, source, line));
        out.push(Event {
            t_us,
            kind: EventKind::Serial {
                text: format!("{line}\n"),
                span_us: SERIAL_SPAN_US,
            },
        });
    }
    out.sort_by_key(|e| e.t_us);
    Ok(out)
}

/// The Python that holds the sim86 package, ter's helper beside it, and
/// the learner's key when ter has it (else the package finds its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Helper {
    pub python: PathBuf,
    pub script: PathBuf,
    pub key: Option<String>,
}

impl Helper {
    /// Run the helper and read its answer. `log`: where to keep what it
    /// printed.
    pub fn call(
        &self,
        args: &[OsString],
        budget: Duration,
        log: Option<&Path>,
    ) -> Result<Answer, Failure> {
        let mut cmd = Command::new(&self.python);
        cmd.arg(&self.script)
            .args(args)
            .env("SIM86_TELEMETRY", "0")
            .env("PYTHONIOENCODING", "utf-8");
        if let Some(key) = &self.key {
            cmd.env(KEY_ENV, key);
        }
        if let Some(dir) = log {
            cmd.current_dir(dir);
        }
        let output = run_with_budget(&mut cmd, budget).map_err(|e| {
            Failure::new(
                FailureKind::Missing,
                format!("could not run {}: {e}", self.python.display()),
            )
        })?;
        if let Some(dir) = log {
            let _ = std::fs::write(dir.join(OUTPUT_LOG), &output.stdout);
            let _ = std::fs::write(dir.join(ERROR_LOG), &output.stderr);
        }
        if matches!(output.status, Ended::Killed) {
            return Err(Failure::new(
                FailureKind::Helper,
                format!(
                    "it did not finish within {} s and was stopped",
                    budget.as_secs()
                ),
            ));
        }
        parse(&String::from_utf8_lossy(&output.stdout)).map_err(|mut failure| {
            if failure.kind == FailureKind::Helper {
                let said = String::from_utf8_lossy(&output.stderr);
                if let Some(line) = said.lines().rev().find(|l| !l.trim().is_empty()) {
                    failure.message = format!("{}: {}", failure.message, line.trim());
                }
            }
            failure
        })
    }

    /// The package and Python versions, and whether the package finds a
    /// key. Asks sim86 nothing.
    pub fn version(&self) -> Result<Answer, Failure> {
        self.call(&["version".into()], Duration::from_secs(30), None)
    }

    /// The account's plan and credits: one call to sim86.
    pub fn balance(&self) -> Result<Balance, Failure> {
        self.call(&["balance".into()], Duration::from_secs(60), None)?
            .balance
            .ok_or_else(|| Failure::new(FailureKind::Api, "sim86 gave no balance"))
    }
}

/// The form sim86 takes a program in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// The ELF as built.
    Elf,
    /// An ESP chip's merged flash image, made with espflash.
    EspMerged { espflash: PathBuf, chip: String },
}

/// sim86 as a venue for one run, in its run folder.
pub struct Sim86 {
    pub helper: Helper,
    /// The exercise's `sim86/board.json`.
    pub board: PathBuf,
    pub run_dir: PathBuf,
    pub plan: Plan,
    pub image: Image,
    pub timeout_ms: u64,
    /// Filled in by the caller: what the capture header carries.
    pub capture: Capture,
    /// The program in the form sim86 takes, once prepared.
    pub program: Option<PathBuf>,
}

impl Sim86 {
    pub const PROVIDES: [Kind; 4] = [Kind::Serial, Kind::Pins, Kind::Reset, Kind::Stimulus];

    /// How long sim86 may spend on `timeout_ms` of simulated time, on its
    /// own clock: the board runs at about real time, slower when a net is
    /// busy, and the recording is read back before the run is stopped.
    pub fn realtime_ms(timeout_ms: u64) -> u64 {
        120_000 + timeout_ms * 10
    }

    /// How long ter waits for the helper: the run, and before it the
    /// upload and the wait for a free simulator.
    pub fn budget(timeout_ms: u64) -> Duration {
        Duration::from_millis(Self::realtime_ms(timeout_ms)) + Duration::from_secs(360)
    }

    /// What the helper is asked to run.
    pub fn request(&self, stimuli: &[Stimulus], program: &Path) -> Value {
        let nets: Vec<&str> = self.plan.watch.iter().map(|w| w.net.as_str()).collect();
        json!({
            "board": self.board,
            "firmware": {self.plan.mcu.as_str(): program},
            "watch": nets,
            "scenario": scenario(stimuli, &self.plan.buttons),
            "timeout_ms": self.timeout_ms,
            "duration_ms": self.timeout_ms + TAIL_MS,
            "realtime_ms": Self::realtime_ms(self.timeout_ms),
        })
    }

    fn failure(&self, failure: &Failure) -> VenueError {
        let read = |f| std::fs::read_to_string(self.run_dir.join(f)).unwrap_or_default();
        VenueError::unavailable(
            failure.explain(),
            format!(
                "{}\n{}{}",
                failure.message,
                read(ERROR_LOG),
                read(OUTPUT_LOG)
            ),
        )
    }
}

impl Venue for Sim86 {
    fn name(&self) -> &'static str {
        "sim86"
    }

    fn provides(&self) -> EventKinds {
        Self::PROVIDES.into()
    }

    fn prepare(&mut self, elf: &Path) -> Result<(), VenueError> {
        self.program = Some(match &self.image {
            Image::Elf => elf.to_path_buf(),
            Image::EspMerged { espflash, chip } => {
                let image = self.run_dir.join(FIRMWARE);
                let args: Vec<OsString> = vec![
                    "save-image".into(),
                    "--chip".into(),
                    chip.into(),
                    "--merge".into(),
                    elf.into(),
                    image.clone().into(),
                ];
                let made = ter_flash::run_tool(espflash, &args, Duration::from_secs(120)).map_err(
                    |e| {
                        VenueError::unavailable(
                            format!("Could not run {}: {e}", espflash.display()),
                            String::new(),
                        )
                    },
                )?;
                if !made.ok || !image.is_file() {
                    return Err(VenueError::unavailable(
                        "espflash could not make the flash image sim86 runs",
                        made.output,
                    ));
                }
                image
            }
        });
        Ok(())
    }

    fn reset(&mut self) -> Result<(), VenueError> {
        // Every simulation starts from reset.
        Ok(())
    }

    fn run(&mut self, stimuli: &[Stimulus], budget: Duration) -> Result<Recording, VenueError> {
        let program = self.program.clone().ok_or_else(|| {
            VenueError::unavailable("The program was not prepared for sim86", String::new())
        })?;
        let request = self.run_dir.join(REQUEST_FILE);
        let text = serde_json::to_string_pretty(&self.request(stimuli, &program))
            .expect("the request serialises");
        std::fs::write(&request, text).map_err(|e| {
            VenueError::unavailable(format!("Could not write {}: {e}", request.display()), "")
        })?;
        let started_at = crate::rfc3339_now();
        let answered =
            self.helper
                .call(&["run".into(), request.into()], budget, Some(&self.run_dir));
        // The image is made from the program in the build cache; a copy
        // per run would only fill the disk.
        let _ = std::fs::remove_file(self.run_dir.join(FIRMWARE));
        let events = answered
            .and_then(|answer| events(&answer, &self.plan, stimuli, self.timeout_ms))
            .map_err(|failure| self.failure(&failure))?;

        let mut capture = self.capture.clone();
        capture.venue = "sim86".into();
        capture.provides = self.provides();
        capture.pins = self.plan.watch.iter().map(|w| w.pin.clone()).collect();
        capture.stimuli = stimuli.to_vec();
        capture.end_us = self.timeout_ms * 1_000;
        capture.started_at = started_at;
        capture.ended_at = crate::rfc3339_now();
        Ok(Recording { capture, events })
    }

    fn recover(&mut self) -> Recovery {
        Recovery::NotNeeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pilot's uFerris board: an LED on D1, a button on D3.
    const UFERRIS: &str = r#"{"name": "ter-uferris-c3-basic",
      "parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"},
                {"id": "led1", "type": "Led", "config": {"color": "red"}},
                {"id": "btn1", "type": "Btn"}],
      "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"],
                ["btn1.L1", "mcu.D3"], ["btn1.L2", "mcu.GND"]],
      "limits": {"duration": "30s"}}"#;
    const UFERRIS_PINS: &str = r#"{"user_led": "mcu.D1", "user_button": {"part": "btn1"},
      "buzzer": "mcu.D2", "level_user_led": {"part": "led1", "key": "intensity"}}"#;
    const NUCLEO: &str = r#"{"parts": [{"id": "mcu", "type": "NucleoF401RE", "firmware": "fw.elf"},
                {"id": "probe", "type": "Led"}],
      "wires": [{"from": "mcu.D13", "to": "probe.+"}, {"from": "probe.-", "to": "mcu.GND"}]}"#;
    const NUCLEO_PINS: &str =
        r#"{"user_led": "mcu.D13", "user_button": {"part": "mcu", "input": "button"}}"#;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn press(at_ms: u64, name: &str) -> Stimulus {
        Stimulus {
            at_ms,
            kind: StimulusKind::Press { name: name.into() },
        }
    }

    fn release(at_ms: u64, name: &str) -> Stimulus {
        Stimulus {
            at_ms,
            kind: StimulusKind::Release { name: name.into() },
        }
    }

    #[test]
    fn watched_pins_become_nets_and_pressed_ones_buttons() {
        let p = plan(
            UFERRIS,
            UFERRIS_PINS,
            &names(&["user_led", "user_led"]),
            &names(&["user_button"]),
        )
        .unwrap();
        assert_eq!(p.mcu, "mcu");
        assert_eq!(
            p.watch,
            vec![Watch {
                pin: "user_led".into(),
                net: "mcu.D1".into()
            }]
        );
        assert_eq!(
            p.buttons["user_button"],
            Button::Part {
                part: "btn1".into()
            }
        );

        let p = plan(
            NUCLEO,
            NUCLEO_PINS,
            &names(&["user_led"]),
            &names(&["user_button"]),
        )
        .unwrap();
        assert_eq!(p.watch[0].net, "mcu.D13");
        assert_eq!(
            p.buttons["user_button"],
            Button::Input {
                part: "mcu".into(),
                input: "button".into()
            }
        );
    }

    #[test]
    fn a_pin_with_no_wire_is_refused_naming_it() {
        // D2 is in the pin map, and no wire in the board takes it anywhere.
        let err = plan(UFERRIS, UFERRIS_PINS, &names(&["buzzer"]), &[]).unwrap_err();
        assert_eq!(
            err,
            WiringError::NoWire {
                pin: "buzzer".into(),
                net: "mcu.D2".into()
            }
        );
        assert!(err.to_string().contains("no wire"), "{err}");
        assert_eq!(err.pin(), Some("buzzer"));
        // A bare board has no net at all.
        let bare = r#"{"parts": [{"id": "mcu", "type": "NucleoF401RE", "firmware": "fw.elf"}], "wires": []}"#;
        assert!(matches!(
            plan(bare, NUCLEO_PINS, &names(&["user_led"]), &[]),
            Err(WiringError::NoWire { .. })
        ));
        // Serial checks need no net.
        assert!(plan(bare, "{}", &[], &[]).unwrap().watch.is_empty());
    }

    #[test]
    fn a_pin_map_that_cannot_serve_the_check_is_named() {
        let watch = |pins: &str, pin: &str| plan(UFERRIS, pins, &names(&[pin]), &[]).unwrap_err();
        assert_eq!(
            watch(UFERRIS_PINS, "ldr"),
            WiringError::NotInPins("ldr".into())
        );
        assert_eq!(
            watch(UFERRIS_PINS, "user_button"),
            WiringError::NotANet("user_button".into())
        );
        assert_eq!(
            watch(r#"{"user_led": "cpu.D1"}"#, "user_led"),
            WiringError::NoPart {
                pin: "user_led".into(),
                part: "cpu".into()
            }
        );
        assert_eq!(
            plan(UFERRIS, UFERRIS_PINS, &[], &names(&["user_led"])).unwrap_err(),
            WiringError::NotAButton("user_led".into())
        );
        assert_eq!(
            plan(UFERRIS, r#"{"b": {"part": "btn9"}}"#, &[], &names(&["b"])).unwrap_err(),
            WiringError::NoPart {
                pin: "b".into(),
                part: "btn9".into()
            }
        );
        assert!(matches!(
            plan("nope", "{}", &[], &[]),
            Err(WiringError::BadBoard(_))
        ));
        assert!(matches!(
            plan(UFERRIS, "[]", &[], &[]),
            Err(WiringError::BadPins(_))
        ));
        let none = r#"{"parts": [{"id": "led1", "type": "Led"}], "wires": []}"#;
        assert_eq!(
            plan(none, "{}", &[], &[]).unwrap_err(),
            WiringError::Programs(0)
        );
    }

    #[test]
    fn presses_become_a_scenario_planned_before_the_board_starts() {
        let part = plan(UFERRIS, UFERRIS_PINS, &[], &names(&["user_button"])).unwrap();
        let stimuli = [press(1500, "user_button"), release(1700, "user_button")];
        assert_eq!(
            scenario(&stimuli, &part.buttons).unwrap(),
            json!({"name": "ter-setup", "events": [
                {"at": "1500ms", "env": {"part": "btn1", "action": "pressed"}},
                {"at": "1700ms", "env": {"part": "btn1", "action": "released"}},
            ]})
        );
        let input = plan(NUCLEO, NUCLEO_PINS, &[], &names(&["user_button"])).unwrap();
        assert_eq!(
            scenario(&stimuli[..1], &input.buttons).unwrap()["events"][0]["env"],
            json!({"part": "mcu", "input": "button", "value": 1.0})
        );
        assert_eq!(scenario(&[], &part.buttons), None);
    }

    /// What the helper printed for the pilot's Nucleo button toggle
    /// (sim86 0.1.4, 2026-11-23), the run id shortened.
    const NUCLEO_RUN: &str = r#"{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4", "run": "01a209b6", "started_ns": 0, "paused_at_start": true, "end_ns": 3000000000, "pins": {"mcu.D13": {"initial": 0, "edges": [[1000003063, 1]], "reads": 0}}, "serial": [], "applied": [{"atNs": 0, "event": "reached"}, {"atNs": 1000000000, "action": "input:button=1", "event": "acted"}, {"atNs": 1000000000, "event": "cleared"}, {"atNs": 1200000000, "action": "input:button=0", "event": "acted"}, {"atNs": 1200000000, "event": "cleared"}], "wall_s": 6.6}"#;

    fn nucleo_plan() -> Plan {
        plan(
            NUCLEO,
            NUCLEO_PINS,
            &names(&["user_led"]),
            &names(&["user_button"]),
        )
        .unwrap()
    }

    #[test]
    fn a_recorded_answer_becomes_events_from_reset() {
        let answer = parse(NUCLEO_RUN).unwrap();
        assert_eq!(answer.sim86.as_deref(), Some("0.1.4"));
        let stimuli = [press(1000, "user_button"), release(1200, "user_button")];
        assert_eq!(
            events(&answer, &nucleo_plan(), &stimuli, 3000).unwrap(),
            vec![
                Event::reset(0),
                Event::pin(0, "user_led", Level::Low),
                // 1000.003063 ms, to the microsecond.
                Event::pin(1_000_003, "user_led", Level::High),
            ]
        );
    }

    #[test]
    fn serial_lines_are_the_program_parts_once_each_and_in_time() {
        let text = r#"{"helper": 1, "ok": true, "started_ns": 0, "paused_at_start": true, "end_ns": 1000000000,
          "pins": {"mcu.D1": {"initial": null, "edges": [[143324763, 1], [243325813, 0], [1000000001, 1]]}},
          "serial": [[129.0, "mcu:usbserial", "I (110) boot: ok"], [129.0, "mcu:uart0", "I (110) boot: ok"],
                     [140.0, "mcu:usbserial", "Hello"], [140.0, "mcu:usbserial", "Hello"],
                     [150.0, "led1", "not the program"], [1001.0, "mcu:usbserial", "late"]],
          "applied": [{"atNs": 0, "event": "reached"}]}"#
            .replace('\n', " ");
        let p = plan(UFERRIS, UFERRIS_PINS, &names(&["user_led"]), &[]).unwrap();
        let got = events(&parse(&text).unwrap(), &p, &[], 1000).unwrap();
        let serial = |t_us, text: &str| Event {
            t_us,
            kind: EventKind::Serial {
                text: text.into(),
                span_us: 1_000,
            },
        };
        assert_eq!(
            got,
            vec![
                Event::reset(0),
                serial(129_000, "I (110) boot: ok\n"),
                // The same port printing a line twice is two lines.
                serial(140_000, "Hello\n"),
                serial(140_000, "Hello\n"),
                // No level is known before the first edge.
                Event::pin(143_325, "user_led", Level::High),
                Event::pin(243_326, "user_led", Level::Low),
            ]
        );
    }

    #[test]
    fn a_run_that_did_not_start_paused_at_zero_is_never_judged() {
        let late = NUCLEO_RUN.replace("\"started_ns\": 0", "\"started_ns\": 742000000");
        let err = events(&parse(&late).unwrap(), &nucleo_plan(), &[], 3000).unwrap_err();
        assert_eq!(err.kind, FailureKind::NotPaused);
        assert!(err.message.contains("742.000 ms"), "{}", err.message);
        let running = NUCLEO_RUN.replace("\"paused_at_start\": true", "\"paused_at_start\": false");
        assert_eq!(
            events(&parse(&running).unwrap(), &nucleo_plan(), &[], 3000)
                .unwrap_err()
                .kind,
            FailureKind::NotPaused
        );
        // An answer with no pins at all, as the helper gives when it stops there.
        let bare =
            r#"{"helper": 1, "ok": true, "run": "r", "started_ns": 900, "paused_at_start": true}"#;
        assert_eq!(
            events(&parse(bare).unwrap(), &nucleo_plan(), &[], 3000)
                .unwrap_err()
                .kind,
            FailureKind::NotPaused
        );
    }

    #[test]
    fn a_short_run_a_missing_net_or_an_unapplied_press_is_never_judged() {
        let answer = parse(NUCLEO_RUN).unwrap();
        let p = nucleo_plan();
        assert_eq!(
            events(&answer, &p, &[], 3500).unwrap_err().kind,
            FailureKind::Ended
        );
        let mut other = p.clone();
        other.watch[0].net = "mcu.PA5".into();
        assert_eq!(
            events(&answer, &other, &[], 3000).unwrap_err().kind,
            FailureKind::Incomplete
        );
        // The press ter planned at 500 ms is not among those sim86 applied.
        let err = events(&answer, &p, &[press(500, "user_button")], 3000).unwrap_err();
        assert_eq!(err.kind, FailureKind::NotApplied);
        assert!(
            err.message.contains("user_button at 500 ms"),
            "{}",
            err.message
        );
        // One past the end of the run is not waited for.
        assert!(events(&answer, &p, &[press(3000, "user_button")], 3000).is_ok());
    }

    #[test]
    fn every_error_kind_is_a_failure_with_its_fix() {
        let failed = |kind: &str, message: &str| {
            parse(&format!(
                r#"{{"helper": 1, "ok": false, "error": {{"kind": "{kind}", "message": "{message}"}}}}"#
            ))
            .unwrap_err()
        };
        let cases = [
            ("no_key", FailureKind::NoKey, "sim86 login"),
            ("auth", FailureKind::Auth, "refused your key"),
            ("credits", FailureKind::Credits, "no credits left"),
            ("board", FailureKind::Board, "sim86/board.json"),
            ("image", FailureKind::Image, "program as built"),
            ("busy", FailureKind::Busy, "busy"),
            ("version", FailureKind::Version, "ter install sim86"),
            ("refused", FailureKind::Refused, "refused to start"),
            ("ended", FailureKind::Ended, "ended before"),
            ("incomplete", FailureKind::Incomplete, "every edge"),
            ("missing", FailureKind::Missing, "ter install sim86"),
            ("api", FailureKind::Api, "sim86 failed"),
            ("something-new", FailureKind::Helper, "helper failed"),
        ];
        for (kind, want, words) in cases {
            let failure = failed(kind, "its own words");
            assert_eq!(failure.kind, want, "{kind}");
            let line = failure.explain();
            assert!(line.contains(words), "{kind}: {line}");
            assert_eq!(line.lines().count(), 1, "{line}");
        }
        // The key never appears: the helper is not given it on its command
        // line, and no message is built from it.
        assert_eq!(
            parse("Traceback (most recent call last):\n  boom\n")
                .unwrap_err()
                .kind,
            FailureKind::Helper
        );
        assert_eq!(parse("").unwrap_err().kind, FailureKind::Helper);
    }

    #[test]
    fn the_balance_answer_reads() {
        let answer = parse(
            r#"{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4", "balance": {"credits": 20184, "plan": "free"}}"#,
        )
        .unwrap();
        assert_eq!(
            answer.balance,
            Some(Balance {
                credits: 20184,
                plan: "free".into()
            })
        );
    }

    #[test]
    fn the_request_names_the_board_the_program_and_the_nets() {
        let venue = Sim86 {
            helper: Helper {
                python: "python".into(),
                script: "ter_sim86.py".into(),
                key: None,
            },
            board: "ex/sim86/board.json".into(),
            run_dir: PathBuf::new(),
            plan: nucleo_plan(),
            image: Image::Elf,
            timeout_ms: 3000,
            capture: Capture::default(),
            program: None,
        };
        let request = venue.request(&[press(1000, "user_button")], Path::new("t/exercise"));
        assert_eq!(
            request,
            json!({
                "board": "ex/sim86/board.json",
                "firmware": {"mcu": "t/exercise"},
                "watch": ["mcu.D13"],
                "scenario": {"name": "ter-setup", "events": [
                    {"at": "1000ms", "env": {"part": "mcu", "input": "button", "value": 1.0}}]},
                "timeout_ms": 3000,
                "duration_ms": 3300,
                "realtime_ms": 150_000,
            })
        );
        assert_eq!(venue.name(), "sim86");
        assert!(venue.provides().contains(&Kind::Stimulus));
    }

    #[test]
    fn the_helper_is_the_script_ter_ships() {
        assert!(HELPER.contains(&format!("HELPER = {HELPER_VERSION}")));
        // It stops every run it starts, whatever happens in between.
        assert!(HELPER.contains("finally:") && HELPER.contains("run.stop()"));
        assert!(HELPER.contains("paused=True"));
    }
}
