//! Real Wokwi runs, kept as `wokwi-cli` wrote them: each folder under
//! tests/fixtures-wokwi holds the exercise's `diagram.json`, the run copy
//! Wokwi simulated (`run-diagram.json`), the scenario, `wokwi-cli`'s
//! stdout (`wokwi.log`), its serial log and VCD, and the `check.yaml` the
//! exercise ships. The reference solution ran in each, so every check
//! passes.
//!
//! gpio-blinky: two 100 ms flashes a second, no serial output of its own.
//! env-first-build: the generated project, a 2 Hz blink and a coloured
//! `INFO - Hello world!` line each pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ter_check::{CheckFile, CheckStatus};
use ter_telemetry::wokwi::{self, Channel};
use ter_telemetry::{Capture, Event, EventKind, Level, Recording};

/// The XIAO ESP32-C3 target's pin map (target.yaml's `pins:`).
fn c3_pins() -> BTreeMap<String, String> {
    [
        ("buzzer", "GPIO4"),
        ("ldr", "GPIO2"),
        ("user_button", "GPIO5"),
        ("user_led", "GPIO3"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures-wokwi")
        .join(name)
}

fn read(dir: &Path, file: &str) -> Vec<u8> {
    std::fs::read(dir.join(file)).unwrap()
}

/// The recording `ter run --sim` makes from these files.
fn replay(name: &str) -> (CheckFile, wokwi::Plan, Recording) {
    let dir = fixture(name);
    let check = CheckFile::parse(&String::from_utf8(read(&dir, "check.yaml")).unwrap()).unwrap();
    let watch: Vec<String> = check
        .checks
        .iter()
        .filter_map(|c| c.pin().map(str::to_string))
        .collect();
    let diagram = String::from_utf8(read(&dir, "diagram.json")).unwrap();
    let plan = wokwi::plan(&diagram, &c3_pins(), &watch, &check.driven_pins()).unwrap();

    let end_ms = check.timeout_ms;
    let mut events = vec![Event::reset(0)];
    events.extend(wokwi::serial_events(
        &read(&dir, "wokwi.log"),
        &read(&dir, "serial.log"),
        end_ms,
    ));
    let vcd = String::from_utf8(read(&dir, "wokwi.vcd")).unwrap();
    events.extend(wokwi::pin_events(&vcd, &plan.channels).unwrap());
    events.retain(|e| e.t_us <= end_ms * 1_000);
    events.sort_by_key(|e| e.t_us);
    let rec = Recording {
        capture: Capture {
            venue: "wokwi".into(),
            target: "xiao-esp32c3-nostd".into(),
            provides: wokwi::Wokwi::PROVIDES.into(),
            pins: plan.channels.iter().map(|c| c.pin.clone()).collect(),
            stimuli: check.stimuli(),
            end_us: end_ms * 1_000,
            ..Capture::default()
        },
        events,
    };
    (check, plan, rec)
}

#[test]
fn the_diagram_ter_writes_is_the_one_wokwi_ran() {
    for name in ["gpio-blinky", "env-first-build"] {
        let (_, plan, _) = replay(name);
        let ran: serde_json::Value =
            serde_json::from_slice(&read(&fixture(name), "run-diagram.json")).unwrap();
        assert_eq!(plan.diagram, ran, "{name}");
        assert_eq!(
            plan.channels,
            vec![Channel {
                index: 0,
                pin: "user_led".into(),
                board_pin: "D1".into()
            }],
            "{name}"
        );
    }
}

#[test]
fn the_real_vcd_maps_to_the_led_by_channel() {
    let (_, _, rec) = replay("gpio-blinky");
    let led: Vec<(u64, Level)> = rec
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::PinEdge { name, level } if name == "user_led" => Some((e.t_us, *level)),
            _ => None,
        })
        .collect();
    // Low at reset; the first flash about 47 ms in, 100 ms wide.
    assert_eq!(led[0], (0, Level::Low));
    assert_eq!(led[1].1, Level::High);
    assert!((40_000..60_000).contains(&led[1].0), "{led:?}");
    assert_eq!(led[2].1, Level::Low);
    let width = led[2].0 - led[1].0;
    assert!((99_900..=100_100).contains(&width), "{width} us");
}

#[test]
fn real_serial_is_timed_and_matches_the_log() {
    let (_, _, rec) = replay("env-first-build");
    let serial: Vec<(u64, u64, &str)> = rec
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Serial { text, span_us } => Some((e.t_us, *span_us, text.as_str())),
            _ => None,
        })
        .collect();
    // The boot ROM and the first greeting come in the first 50 ms step,
    // and the line break wokwi-cli prints before its first marker is not
    // the program's.
    let (t, span, text) = serial[0];
    assert_eq!((t, span), (0, 50_000));
    assert!(text.starts_with("ESP-ROM:"), "{text:?}");
    assert!(text.contains("INFO - Hello world!"), "{text:?}");
    let joined: String = serial.iter().map(|s| s.2).collect();
    assert_eq!(
        joined.as_bytes(),
        read(&fixture("env-first-build"), "serial.log"),
        "every byte the log has, and no other"
    );
    assert!(!rec.serial_text().contains('\u{1b}'), "no colour codes");
}

#[test]
fn the_reference_solutions_pass_every_check() {
    for name in ["gpio-blinky", "env-first-build"] {
        let (check, _, rec) = replay(name);
        let verdicts = ter_check::evaluate(&check, &rec);
        assert!(!verdicts.is_empty());
        for v in &verdicts {
            assert_eq!(v.status, CheckStatus::Pass, "{name}: {v:?}");
        }
    }
}
