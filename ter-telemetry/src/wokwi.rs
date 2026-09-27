//! The Wokwi venue: the exercise's circuit in Wokwi's simulator, run
//! headless by `wokwi-cli` with the learner's own Wokwi token.
//!
//! Pins: the curriculum's `diagram.json` files carry no logic analyzer,
//! and `wokwi-cli` writes a VCD only when the diagram has one. So the run
//! copy of the diagram (never the learner's file) gets one
//! `wokwi-logic-analyzer`, with channels D0..D7 wired to the board pins the
//! checks name, one channel per distinct pin. The VCD's signals are mapped
//! back to the checks' pin names by channel.
//!
//! Stimulus: a Wokwi scenario presses and releases the button wired to the
//! named pin at the times `check.yaml` gives.
//!
//! Serial: `wokwi-cli` prints what the module sends but not when. The
//! scenario advances the simulation in fixed steps, and each step prints a
//! marker naming its simulated time, so bytes that come between two markers
//! were sent between those two times. They are recorded with that span.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::event::{Event, EventKind, EventKinds, Kind, Level, Stimulus, StimulusKind};
use crate::recording::{Capture, Recording};
use crate::vcd;
use crate::venue::{Recovery, Venue, VenueError};

/// The logic analyzer has eight channels.
pub const MAX_PINS: usize = 8;
/// The part id of the analyzer ter adds.
const ANALYZER_ID: &str = "ter_logic";
/// Serial timing resolution: the scenario advances this far per step.
pub const STEP_MS: u64 = 50;
/// The scenario's name, which prefixes every line it prints.
const SCENARIO: &str = "ter-clock";
/// `wokwi-cli`'s exit code when the simulation reached `--timeout`: how a
/// full-length run ends.
const TIMEOUT_EXIT: i32 = 42;

pub const RUN_DIAGRAM: &str = "diagram.json";
pub const SCENARIO_FILE: &str = "scenario.yaml";
pub const VCD_FILE: &str = "wokwi.vcd";
pub const SERIAL_LOG: &str = "serial.log";
/// What `wokwi-cli` printed: stdout (serial and step markers), stderr.
pub const OUTPUT_LOG: &str = "wokwi.log";
pub const ERROR_LOG: &str = "wokwi.err";
pub const FIRMWARE: &str = "firmware.elf";

/// A Wokwi board part: which of its pins is which GPIO, and its ground.
struct Board {
    part_type: &'static str,
    ground: &'static str,
    pins: &'static [(&'static str, &'static str)],
}

/// Wokwi's own board definitions (wokwi/wokwi-boards), as `(pin, GPIO)`.
const BOARDS: &[Board] = &[Board {
    part_type: "board-xiao-esp32-c3",
    ground: "GND",
    pins: &[
        ("D0", "GPIO2"),
        ("D1", "GPIO3"),
        ("D2", "GPIO4"),
        ("D3", "GPIO5"),
        ("D4", "GPIO6"),
        ("D5", "GPIO7"),
        ("D6", "GPIO21"),
        ("D7", "GPIO20"),
        ("D8", "GPIO8"),
        ("D9", "GPIO9"),
        ("D10", "GPIO10"),
    ],
}];

impl Board {
    fn pin_for(&self, gpio: &str) -> Option<&'static str> {
        self.pins
            .iter()
            .find(|(_, g)| g.eq_ignore_ascii_case(gpio))
            .map(|(p, _)| *p)
    }
}

/// Why a circuit cannot show what a check needs. Found before anything
/// builds; each names what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WiringError {
    BadDiagram(String),
    NoBoard,
    UnknownBoard(String),
    /// The target's pin map does not name this pin.
    NotInPinMap(String),
    /// The pin's GPIO is not brought out on the simulated board.
    NotOnBoard {
        pin: String,
        gpio: String,
        board: String,
    },
    /// More distinct pins than analyzer channels, in check order.
    TooManyPins(Vec<String>),
    /// No pushbutton on the pin a stimulus presses.
    NoButton {
        pin: String,
        board_pin: String,
    },
}

impl std::fmt::Display for WiringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WiringError::BadDiagram(e) => write!(f, "diagram.json is not a Wokwi diagram: {e}"),
            WiringError::NoBoard => write!(f, "diagram.json has no board part"),
            WiringError::UnknownBoard(t) => {
                write!(f, "ter does not know the pins of Wokwi's {t}")
            }
            WiringError::NotInPinMap(p) => {
                write!(f, "the target's pin map has no pin named {p}")
            }
            WiringError::NotOnBoard { pin, gpio, board } => {
                write!(f, "{pin} is {gpio}, which {board} does not bring out")
            }
            WiringError::TooManyPins(pins) => write!(
                f,
                "the checks watch {} pins ({}), and Wokwi's logic analyzer has {MAX_PINS} channels",
                pins.len(),
                pins.join(", ")
            ),
            WiringError::NoButton { pin, board_pin } => write!(
                f,
                "no pushbutton in diagram.json is wired to {board_pin}, the pin {pin} is on"
            ),
        }
    }
}

/// One analyzer channel: `D<index>` watches `pin`, on `board_pin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub index: usize,
    pub pin: String,
    pub board_pin: String,
}

/// The run copy of a diagram, and what ter added to it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub diagram: Value,
    pub channels: Vec<Channel>,
    /// Pin name to the id of the pushbutton on it.
    pub buttons: BTreeMap<String, String>,
}

/// Wire the analyzer and find the buttons. `watch` are the pins the checks
/// name, in check order (repeats allowed); `press` the pins the stimuli
/// drive; `pin_map` the target's pin names to GPIOs.
pub fn plan(
    diagram_json: &str,
    pin_map: &BTreeMap<String, String>,
    watch: &[String],
    press: &[String],
) -> Result<Plan, WiringError> {
    let mut diagram: Value =
        serde_json::from_str(diagram_json).map_err(|e| WiringError::BadDiagram(e.to_string()))?;
    let parts = diagram
        .get("parts")
        .and_then(Value::as_array)
        .ok_or_else(|| WiringError::BadDiagram("no parts list".into()))?;
    let (board_id, board) = find_board(parts)?;

    let mut distinct: Vec<&String> = Vec::new();
    for pin in watch {
        if !distinct.contains(&pin) {
            distinct.push(pin);
        }
    }
    if distinct.len() > MAX_PINS {
        return Err(WiringError::TooManyPins(
            distinct.into_iter().cloned().collect(),
        ));
    }
    let board_pin = |pin: &String| -> Result<&'static str, WiringError> {
        let gpio = pin_map
            .get(pin)
            .ok_or_else(|| WiringError::NotInPinMap(pin.clone()))?;
        board.pin_for(gpio).ok_or_else(|| WiringError::NotOnBoard {
            pin: pin.clone(),
            gpio: gpio.clone(),
            board: board.part_type.into(),
        })
    };
    let channels = distinct
        .into_iter()
        .enumerate()
        .map(|(index, pin)| {
            Ok(Channel {
                index,
                pin: pin.clone(),
                board_pin: board_pin(pin)?.into(),
            })
        })
        .collect::<Result<Vec<_>, WiringError>>()?;

    let nets = Nets::of(&diagram);
    let mut buttons = BTreeMap::new();
    for pin in press {
        let on = board_pin(pin)?;
        let id = nets
            .parts_on(&format!("{board_id}:{on}"))
            .into_iter()
            .find(|part| {
                parts.iter().any(|p| {
                    p["id"] == *part
                        && p["type"]
                            .as_str()
                            .is_some_and(|t| t.starts_with("wokwi-pushbutton"))
                })
            })
            .ok_or_else(|| WiringError::NoButton {
                pin: pin.clone(),
                board_pin: on.into(),
            })?;
        buttons.insert(pin.clone(), id);
    }

    if !channels.is_empty() {
        let id = unique_id(parts, ANALYZER_ID);
        let parts = diagram["parts"].as_array_mut().expect("checked above");
        parts.push(json!({
            "type": "wokwi-logic-analyzer",
            "id": id,
            "top": -150,
            "left": 0,
            "attrs": {},
        }));
        if !diagram["connections"].is_array() {
            diagram["connections"] = json!([]);
        }
        let connections = diagram["connections"].as_array_mut().expect("just set");
        for ch in &channels {
            connections.push(json!([
                format!("{board_id}:{}", ch.board_pin),
                format!("{id}:D{}", ch.index),
                "violet",
                []
            ]));
        }
        connections.push(json!([
            format!("{board_id}:{}", board.ground),
            format!("{id}:GND"),
            "black",
            []
        ]));
    }
    Ok(Plan {
        diagram,
        channels,
        buttons,
    })
}

fn find_board(parts: &[Value]) -> Result<(String, &'static Board), WiringError> {
    let mut unknown = None;
    for p in parts {
        let Some(t) = p["type"].as_str() else {
            continue;
        };
        if let Some(board) = BOARDS.iter().find(|b| b.part_type == t) {
            let id = p["id"]
                .as_str()
                .ok_or_else(|| WiringError::BadDiagram(format!("the {t} part has no id")))?;
            return Ok((id.to_string(), board));
        }
        if t.starts_with("board-") {
            unknown.get_or_insert_with(|| t.to_string());
        }
    }
    Err(unknown.map_or(WiringError::NoBoard, WiringError::UnknownBoard))
}

fn unique_id(parts: &[Value], base: &str) -> String {
    let taken = |id: &str| parts.iter().any(|p| p["id"] == id);
    let mut id = base.to_string();
    let mut n = 2;
    while taken(&id) {
        id = format!("{base}{n}");
        n += 1;
    }
    id
}

/// Which part pins are connected, from the diagram's wires.
struct Nets {
    parent: HashMap<String, String>,
}

impl Nets {
    fn of(diagram: &Value) -> Self {
        let mut nets = Nets {
            parent: HashMap::new(),
        };
        for c in diagram["connections"].as_array().into_iter().flatten() {
            if let (Some(a), Some(b)) = (c[0].as_str(), c[1].as_str()) {
                let (ra, rb) = (nets.root(a), nets.root(b));
                if ra != rb {
                    nets.parent.insert(ra, rb);
                }
            }
        }
        nets
    }

    fn root(&mut self, pin: &str) -> String {
        let mut at = pin.to_string();
        while let Some(up) = self.parent.get(&at) {
            at = up.clone();
        }
        at
    }

    /// The parts with a pin on the same net as `pin` (`part:pin`).
    fn parts_on(&self, pin: &str) -> Vec<String> {
        let root = |p: &str| {
            let mut at = p.to_string();
            while let Some(up) = self.parent.get(&at) {
                at = up.clone();
            }
            at
        };
        let net = root(pin);
        let mut parts: Vec<String> = self
            .parent
            .keys()
            .chain(self.parent.values())
            .filter(|p| *p != pin && root(p) == net)
            .filter_map(|p| p.split_once(':').map(|(part, _)| part.to_string()))
            .collect();
        parts.sort();
        parts.dedup();
        parts
    }
}

/// The scenario that applies `stimuli` and paces the simulation in
/// [`STEP_MS`] steps up to `timeout_ms`. Every step prints `@<ms>`, its
/// simulated start time. The last step runs past `timeout_ms`, so the run
/// always ends on `wokwi-cli`'s timeout, at `timeout_ms` exactly.
pub fn scenario(
    stimuli: &[Stimulus],
    buttons: &BTreeMap<String, String>,
    timeout_ms: u64,
) -> String {
    let mut times: Vec<u64> = (0..timeout_ms).step_by(STEP_MS as usize).collect();
    times.extend(stimuli.iter().map(|s| s.at_ms).filter(|t| *t < timeout_ms));
    times.sort_unstable();
    times.dedup();

    let mut out = format!("name: {SCENARIO}\nversion: 1\nauthor: ter\nsteps:\n");
    for (i, &t) in times.iter().enumerate() {
        for s in stimuli.iter().filter(|s| s.at_ms == t) {
            let (name, value) = match &s.kind {
                StimulusKind::Press { name } => (name, 1),
                StimulusKind::Release { name } => (name, 0),
                // Wokwi has no power; a reset is not in check.yaml's vocabulary.
                StimulusKind::Reset | StimulusKind::Power { .. } => continue,
            };
            if let Some(part) = buttons.get(name) {
                out.push_str(&format!(
                    "  - set-control:\n      part-id: {part}\n      control: pressed\n      value: {value}\n"
                ));
            }
        }
        let next = times.get(i + 1).copied().unwrap_or(timeout_ms + 1_000);
        out.push_str(&format!("  - name: \"@{t}\"\n    delay: {}ms\n", next - t));
    }
    out
}

/// The serial bytes in `stdout`, each chunk timed by the step markers
/// around it. `serial_log` is what `wokwi-cli` wrote to its serial log: the
/// same bytes without markers, which settles where a marker's line break
/// was the program's and where `wokwi-cli` added it. `end_ms` closes the
/// last step.
pub fn serial_events(stdout: &[u8], serial_log: &[u8], end_ms: u64) -> Vec<Event> {
    let prefix = format!("[{SCENARIO}] ");
    let mut segments: Vec<(u64, &[u8])> = Vec::new();
    let mut now_ms = 0;
    let mut seg_start = 0;
    let mut i = 0;
    while i < stdout.len() {
        let line_start = i == 0 || stdout[i - 1] == b'\n';
        if line_start && stdout[i..].starts_with(prefix.as_bytes()) {
            let end = stdout[i..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(stdout.len(), |p| i + p + 1);
            segments.push((now_ms, &stdout[seg_start..i]));
            let line = String::from_utf8_lossy(&stdout[i + prefix.len()..end]);
            if let Some(t) = line
                .trim()
                .strip_prefix("Executing step: @")
                .and_then(|t| t.parse::<u64>().ok())
            {
                now_ms = t;
            }
            seg_start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    segments.push((now_ms, &stdout[seg_start..]));

    // Match each segment against the log, which holds the program's bytes
    // exactly: a segment may end in a line break wokwi-cli added before
    // its marker. Then join the segments of one step.
    let mut pos = 0;
    let mut steps: Vec<(u64, Vec<u8>)> = Vec::new();
    for (t, bytes) in segments {
        let rest = &serial_log[pos.min(serial_log.len())..];
        let text: &[u8] = match bytes.strip_suffix(b"\n") {
            _ if rest.starts_with(bytes) => bytes,
            Some(trimmed) if rest.starts_with(trimmed) => trimmed,
            // Not what the log says; keep the bytes as printed.
            _ => bytes,
        };
        if rest.starts_with(text) {
            pos += text.len();
        }
        match steps.last_mut() {
            Some((last, buf)) if *last == t => buf.extend_from_slice(text),
            _ => steps.push((t, text.to_vec())),
        }
    }

    // Each step's bytes were sent between its start and the next step's.
    let mut events = Vec::new();
    for (k, (t, text)) in steps.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let until = steps.get(k + 1).map_or(end_ms, |(next, _)| *next).max(*t);
        events.push(Event {
            t_us: t * 1_000,
            kind: EventKind::Serial {
                text: String::from_utf8_lossy(text).into_owned(),
                span_us: (until - t) * 1_000,
            },
        });
    }
    if pos < serial_log.len() {
        let t = steps.last().map_or(0, |(t, _)| *t);
        events.push(Event {
            t_us: t * 1_000,
            kind: EventKind::Serial {
                text: String::from_utf8_lossy(&serial_log[pos..]).into_owned(),
                span_us: end_ms.saturating_sub(t) * 1_000,
            },
        });
    }
    events
}

/// The pin events in a VCD from the analyzer ter wired: channel `D<n>` is
/// `channels[n]`'s pin. Unknown levels are left out, and so is a sample
/// that repeats a pin's level.
pub fn pin_events(vcd_text: &str, channels: &[Channel]) -> Result<Vec<Event>, String> {
    let by_signal: HashMap<String, &str> = channels
        .iter()
        .map(|c| (format!("D{}", c.index), c.pin.as_str()))
        .collect();
    let mut last: HashMap<&str, Level> = HashMap::new();
    let mut events = Vec::new();
    for change in vcd::parse(vcd_text)? {
        let (Some(pin), Some(level)) = (by_signal.get(&change.signal), change.level) else {
            continue;
        };
        if last.insert(pin, level) == Some(level) {
            continue;
        }
        events.push(Event::pin(change.t_ns / 1_000, pin, level));
    }
    Ok(events)
}

/// Wokwi as a venue for one run, in its run folder.
pub struct Wokwi {
    pub cli: PathBuf,
    pub token: String,
    pub run_dir: PathBuf,
    pub plan: Plan,
    pub timeout_ms: u64,
    /// Filled in by the caller: what the capture header carries.
    pub capture: Capture,
}

impl Wokwi {
    pub const PROVIDES: [Kind; 4] = [Kind::Serial, Kind::Pins, Kind::Reset, Kind::Stimulus];

    fn failure(&self, message: String) -> VenueError {
        let read = |f| std::fs::read_to_string(self.run_dir.join(f)).unwrap_or_default();
        VenueError::unavailable(message, format!("{}{}", read(ERROR_LOG), read(OUTPUT_LOG)))
    }
}

impl Venue for Wokwi {
    fn name(&self) -> &'static str {
        "wokwi"
    }

    fn provides(&self) -> EventKinds {
        Self::PROVIDES.into()
    }

    fn prepare(&mut self, elf: &Path) -> Result<(), VenueError> {
        let io = |what: &str, e: std::io::Error| {
            VenueError::unavailable(format!("Could not {what}: {e}"), String::new())
        };
        std::fs::copy(elf, self.run_dir.join(FIRMWARE)).map_err(|e| io("copy the program", e))?;
        let diagram = serde_json::to_string_pretty(&self.plan.diagram).expect("diagram serialises");
        std::fs::write(self.run_dir.join(RUN_DIAGRAM), diagram)
            .map_err(|e| io("write the run's diagram.json", e))
    }

    fn reset(&mut self) -> Result<(), VenueError> {
        // Every simulation starts from reset.
        Ok(())
    }

    fn run(&mut self, stimuli: &[Stimulus], budget: Duration) -> Result<Recording, VenueError> {
        let dir = &self.run_dir;
        std::fs::write(
            dir.join(SCENARIO_FILE),
            scenario(stimuli, &self.plan.buttons, self.timeout_ms),
        )
        .map_err(|e| self.failure(format!("Could not write the scenario: {e}")))?;
        let watch_pins = !self.plan.channels.is_empty();

        let mut cmd = Command::new(&self.cli);
        cmd.arg(dir)
            .args(["--timeout", &self.timeout_ms.to_string()])
            .arg("--scenario")
            .arg(dir.join(SCENARIO_FILE))
            .arg("--serial-log-file")
            .arg(dir.join(SERIAL_LOG))
            .arg("--elf")
            .arg(dir.join(FIRMWARE))
            .arg("--quiet");
        if watch_pins {
            cmd.arg("--vcd-file").arg(dir.join(VCD_FILE));
        }
        let started_at = crate::rfc3339_now();
        let output = run_with_budget(
            cmd.env("WOKWI_CLI_TOKEN", &self.token)
                .env("NO_COLOR", "1")
                .env("FORCE_COLOR", "0"),
            budget,
        )
        .map_err(|e| {
            VenueError::unavailable(format!("Could not run {}: {e}", self.cli.display()), "")
        })?;
        let _ = std::fs::write(dir.join(OUTPUT_LOG), &output.stdout);
        let _ = std::fs::write(dir.join(ERROR_LOG), &output.stderr);
        // The program is in the build cache and its hash in the capture; a
        // copy per run would only fill the disk.
        let _ = std::fs::remove_file(dir.join(FIRMWARE));

        match output.status {
            Ended::Exited(0 | TIMEOUT_EXIT) => {}
            Ended::Exited(code) => {
                let said = String::from_utf8_lossy(&output.stderr);
                let said = said.trim();
                return Err(self.failure(format!(
                    "wokwi-cli stopped with exit code {code}{}",
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", last_lines(said, 6))
                    }
                )));
            }
            Ended::Killed => {
                return Err(self.failure(format!(
                    "wokwi-cli did not finish within {} s and was stopped",
                    budget.as_secs()
                )));
            }
        }

        let serial_log = std::fs::read(dir.join(SERIAL_LOG)).unwrap_or_default();
        let mut events = vec![Event::reset(0)];
        events.extend(serial_events(&output.stdout, &serial_log, self.timeout_ms));
        if watch_pins {
            let vcd = std::fs::read_to_string(dir.join(VCD_FILE)).map_err(|e| {
                self.failure(format!("wokwi-cli wrote no logic analyzer capture: {e}"))
            })?;
            events.extend(
                pin_events(&vcd, &self.plan.channels)
                    .map_err(|e| self.failure(format!("The logic analyzer capture: {e}")))?,
            );
        }
        let end_us = self.timeout_ms * 1_000;
        events.retain(|e| e.t_us <= end_us);
        events.sort_by_key(|e| e.t_us);

        let mut capture = self.capture.clone();
        capture.venue = "wokwi".into();
        capture.provides = self.provides();
        capture.pins = self.plan.channels.iter().map(|c| c.pin.clone()).collect();
        capture.stimuli = stimuli.to_vec();
        capture.end_us = end_us;
        capture.started_at = started_at;
        capture.ended_at = crate::rfc3339_now();
        Ok(Recording { capture, events })
    }

    fn recover(&mut self) -> Recovery {
        Recovery::NotNeeded
    }
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join(" ")
}

enum Ended {
    Exited(i32),
    Killed,
}

struct Output {
    status: Ended,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `cmd`, stopping it after `budget` of wall time.
fn run_with_budget(cmd: &mut Command, budget: Duration) -> std::io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    };
    let out = drain(Box::new(child.stdout.take().expect("piped")));
    let err = drain(Box::new(child.stderr.take().expect("piped")));
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Ended::Exited(s.code().unwrap_or(-1));
        }
        if started.elapsed() > budget {
            let _ = child.kill();
            let _ = child.wait();
            break Ended::Killed;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The course's uFerris circuit, as the curriculum ships it (trimmed).
    const UFERRIS: &str = r#"{
      "version": 1,
      "parts": [
        {"type": "board-xiao-esp32-c3", "id": "xiao", "top": 60, "left": 0, "attrs": {}},
        {"type": "wokwi-led", "id": "led1", "attrs": {"color": "red"}},
        {"type": "wokwi-resistor", "id": "r1", "attrs": {"value": "220"}},
        {"type": "wokwi-pushbutton", "id": "btn1", "attrs": {"bounce": "0"}},
        {"type": "wokwi-buzzer", "id": "bz1", "attrs": {}}
      ],
      "connections": [
        ["xiao:D1", "r1:1", "green", ["h20"]],
        ["r1:2", "led1:A", "green", []],
        ["led1:C", "xiao:GND", "black", []],
        ["btn1:1.l", "xiao:D3", "orange", []],
        ["btn1:2.l", "xiao:GND", "black", []],
        ["bz1:2", "xiao:D2", "magenta", []],
        ["bz1:1", "xiao:GND", "black", []]
      ],
      "dependencies": {}
    }"#;

    fn pin_map() -> BTreeMap<String, String> {
        [
            ("user_led", "GPIO3"),
            ("user_button", "GPIO5"),
            ("buzzer", "GPIO4"),
            ("ldr", "GPIO2"),
            ("far", "GPIO18"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_analyzer_is_wired_to_each_distinct_checked_pin() {
        let original: Value = serde_json::from_str(UFERRIS).unwrap();
        let plan = plan(
            UFERRIS,
            &pin_map(),
            &names(&["user_led", "user_led", "buzzer", "user_led"]),
            &[],
        )
        .unwrap();

        assert_eq!(
            plan.channels,
            vec![
                Channel {
                    index: 0,
                    pin: "user_led".into(),
                    board_pin: "D1".into()
                },
                Channel {
                    index: 1,
                    pin: "buzzer".into(),
                    board_pin: "D2".into()
                },
            ]
        );

        let parts = plan.diagram["parts"].as_array().unwrap();
        let analyzers: Vec<_> = parts
            .iter()
            .filter(|p| p["type"] == "wokwi-logic-analyzer")
            .collect();
        assert_eq!(analyzers.len(), 1, "exactly one analyzer");
        assert_eq!(analyzers[0]["id"], "ter_logic");
        assert_eq!(parts.len(), original["parts"].as_array().unwrap().len() + 1);

        let connections = plan.diagram["connections"].as_array().unwrap();
        let original_connections = original["connections"].as_array().unwrap();
        assert_eq!(
            &connections[..original_connections.len()],
            &original_connections[..],
            "the circuit itself is untouched"
        );
        let added: Vec<(&str, &str)> = connections[original_connections.len()..]
            .iter()
            .map(|c| (c[0].as_str().unwrap(), c[1].as_str().unwrap()))
            .collect();
        assert_eq!(
            added,
            vec![
                ("xiao:D1", "ter_logic:D0"),
                ("xiao:D2", "ter_logic:D1"),
                ("xiao:GND", "ter_logic:GND"),
            ]
        );
        // The learner's copy is text ter never writes; the plan is a copy.
        assert_eq!(serde_json::from_str::<Value>(UFERRIS).unwrap(), original);
    }

    #[test]
    fn no_pin_checks_means_no_analyzer() {
        let plan = plan(UFERRIS, &pin_map(), &[], &[]).unwrap();
        assert!(plan.channels.is_empty());
        assert_eq!(
            plan.diagram,
            serde_json::from_str::<Value>(UFERRIS).unwrap()
        );
    }

    #[test]
    fn more_than_eight_pins_is_refused_naming_them() {
        let many = r#"{"parts":[{"type":"board-xiao-esp32-c3","id":"xiao"}],"connections":[]}"#;
        let map: BTreeMap<String, String> = (0..=10)
            .map(|n| {
                let gpio = BOARDS[0].pins[n].1;
                (format!("p{n}"), gpio.to_string())
            })
            .collect();
        let watch: Vec<String> = (0..9).map(|n| format!("p{n}")).collect();
        assert_eq!(
            plan(many, &map, &watch, &[]).unwrap_err(),
            WiringError::TooManyPins(watch.clone())
        );
        let err = plan(many, &map, &watch, &[]).unwrap_err().to_string();
        assert!(
            err.contains("9 pins") && err.contains("8 channels"),
            "{err}"
        );
        assert!(plan(many, &map, &watch[..8], &[]).is_ok(), "eight fit");
    }

    #[test]
    fn a_pin_off_the_map_or_off_the_board_is_named() {
        assert_eq!(
            plan(UFERRIS, &pin_map(), &names(&["nope"]), &[]).unwrap_err(),
            WiringError::NotInPinMap("nope".into())
        );
        assert_eq!(
            plan(UFERRIS, &pin_map(), &names(&["far"]), &[]).unwrap_err(),
            WiringError::NotOnBoard {
                pin: "far".into(),
                gpio: "GPIO18".into(),
                board: "board-xiao-esp32-c3".into()
            }
        );
    }

    #[test]
    fn an_unknown_board_or_none_is_refused() {
        let other = r#"{"parts":[{"type":"board-pi-pico","id":"pico"}],"connections":[]}"#;
        assert_eq!(
            plan(other, &pin_map(), &[], &[]).unwrap_err(),
            WiringError::UnknownBoard("board-pi-pico".into())
        );
        let none = r#"{"parts":[],"connections":[]}"#;
        assert_eq!(
            plan(none, &pin_map(), &[], &[]).unwrap_err(),
            WiringError::NoBoard
        );
        assert!(matches!(
            plan("not json", &pin_map(), &[], &[]),
            Err(WiringError::BadDiagram(_))
        ));
    }

    #[test]
    fn a_press_finds_the_button_on_the_pin() {
        let plan = plan(UFERRIS, &pin_map(), &[], &names(&["user_button"])).unwrap();
        assert_eq!(plan.buttons["user_button"], "btn1");
        assert_eq!(
            super::plan(UFERRIS, &pin_map(), &[], &names(&["buzzer"])).unwrap_err(),
            WiringError::NoButton {
                pin: "buzzer".into(),
                board_pin: "D2".into()
            }
        );
    }

    #[test]
    fn an_analyzer_id_already_taken_gets_another() {
        let taken = r#"{"parts":[{"type":"board-xiao-esp32-c3","id":"xiao"},
            {"type":"wokwi-led","id":"ter_logic"}],"connections":[]}"#;
        let plan = plan(taken, &pin_map(), &names(&["user_led"]), &[]).unwrap();
        let conns = plan.diagram["connections"].as_array().unwrap();
        assert_eq!(conns[0][1], "ter_logic2:D0");
    }

    #[test]
    fn the_scenario_steps_to_the_timeout_and_presses_on_time() {
        let buttons: BTreeMap<_, _> = [("user_button".to_string(), "btn1".to_string())].into();
        let stimuli = vec![
            Stimulus {
                at_ms: 120,
                kind: StimulusKind::Press {
                    name: "user_button".into(),
                },
            },
            Stimulus {
                at_ms: 150,
                kind: StimulusKind::Release {
                    name: "user_button".into(),
                },
            },
        ];
        let s = scenario(&stimuli, &buttons, 200);
        assert_eq!(
            s,
            concat!(
                "name: ter-clock\nversion: 1\nauthor: ter\nsteps:\n",
                "  - name: \"@0\"\n    delay: 50ms\n",
                "  - name: \"@50\"\n    delay: 50ms\n",
                "  - name: \"@100\"\n    delay: 20ms\n",
                "  - set-control:\n      part-id: btn1\n      control: pressed\n      value: 1\n",
                "  - name: \"@120\"\n    delay: 30ms\n",
                "  - set-control:\n      part-id: btn1\n      control: pressed\n      value: 0\n",
                "  - name: \"@150\"\n    delay: 1050ms\n",
            )
        );
    }

    #[test]
    fn serial_is_timed_by_the_step_it_arrived_in() {
        let stdout = b"[ter-clock] Executing step: @0\n[ter-clock] delay 50ms\n\
boot ok\n[ter-clock] Executing step: @50\n[ter-clock] delay 50ms\n\
Read\n[ter-clock] Executing step: @100\n[ter-clock] delay 1050ms\ning: 5\n";
        // wokwi-cli added the line break before the @100 marker ("Read" had
        // none), and the log shows it.
        let log = b"boot ok\nReading: 5\n";
        let events = serial_events(stdout, log, 1_000);
        let serial: Vec<(u64, &str, u64)> = events
            .iter()
            .map(|e| match &e.kind {
                EventKind::Serial { text, span_us } => (e.t_us, text.as_str(), *span_us),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            serial,
            vec![
                (0, "boot ok\n", 50_000),
                (50_000, "Read", 50_000),
                (100_000, "ing: 5\n", 900_000),
            ]
        );
    }

    #[test]
    fn serial_left_over_in_the_log_is_kept() {
        let stdout = b"[ter-clock] Executing step: @0\nabc";
        let events = serial_events(stdout, b"abcdef", 100);
        let texts: String = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Serial { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, "abcdef");
    }

    #[test]
    fn vcd_signals_map_back_to_pin_names_by_channel() {
        let vcd = "$timescale 1ns $end\n$scope module logic $end\n\
$var wire 1 ! D0 $end\n$var wire 1 \" D1 $end\n$var wire 1 # D2 $end\n\
$upscope $end\n$enddefinitions $end\n#0\n0!\n1\"\n0#\n#1000000\n1!\n#1500000\n1!\n#2000000\n0!\n0\"\n";
        let channels = vec![
            Channel {
                index: 0,
                pin: "user_led".into(),
                board_pin: "D1".into(),
            },
            Channel {
                index: 1,
                pin: "buzzer".into(),
                board_pin: "D2".into(),
            },
        ];
        assert_eq!(
            pin_events(vcd, &channels).unwrap(),
            vec![
                Event::pin(0, "user_led", Level::Low),
                Event::pin(0, "buzzer", Level::High),
                Event::pin(1_000, "user_led", Level::High),
                Event::pin(2_000, "user_led", Level::Low),
                Event::pin(2_000, "buzzer", Level::Low),
            ],
            "D2 has no channel, and the repeated high at 1.5 ms is not a change"
        );
    }
}
