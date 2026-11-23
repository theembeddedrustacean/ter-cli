//! The simulation venue for `ter run --sim`: Wokwi, with the learner's own
//! token and `wokwi-cli`.
//!
//! Everything that can stop a simulated run is settled here, before the
//! build: whether every check can be seen in the circuit (section 6.2 of
//! the design: a check simulation cannot see is an authoring bug, refused
//! as `not_observable`), and whether Wokwi can be reached at all
//! (`venue_unavailable`, nothing posted).

use std::path::{Path, PathBuf};

use ter_check::CheckFile;
use ter_sdk::project::TerToml;
use ter_telemetry::wokwi::{self, Chip, Plan, WiringError};
use ter_telemetry::{EventKinds, Kind};

use crate::output::CliError;
use crate::token_store::TokenStore;

pub const WOKWI_CLI: &str = "wokwi-cli";
/// Where Wokwi's own installer puts `wokwi-cli`, under the home folder.
pub const WOKWI_HOME_BIN: &str = ".wokwi/bin";
pub const TOKEN_ENV: &str = "WOKWI_CLI_TOKEN";
pub use ter_telemetry::wokwi::TOKEN_PAGE;

/// A simulated run, ready to go once the program is built.
pub struct Ready {
    pub cli: PathBuf,
    pub token: String,
    pub plan: Plan,
}

/// The checks `provides` cannot see, each with the kinds it misses.
pub fn unobservable(file: &CheckFile, provides: &EventKinds) -> Vec<(String, Vec<Kind>)> {
    file.checks
        .iter()
        .filter_map(|c| {
            let missing: Vec<Kind> = file
                .needs(c)
                .into_iter()
                .filter(|k| !provides.contains(k))
                .collect();
            (!missing.is_empty()).then(|| (c.id.clone(), missing))
        })
        .collect()
}

fn not_observable(message: String) -> CliError {
    CliError::new("not_observable", message)
}

/// Wire the circuit for `file`'s checks: the run copy of the exercise's
/// `diagram.json` with a logic analyzer on the checked pins.
pub fn plan(dir: &Path, ter: &TerToml, file: &CheckFile) -> Result<Plan, CliError> {
    if !has_circuit(dir) {
        return Err(no_circuit(ter));
    }
    let provides: EventKinds = wokwi::Wokwi::PROVIDES.into();
    if let Some((id, missing)) = unobservable(file, &provides).into_iter().next() {
        let kinds: Vec<String> = missing.iter().map(ToString::to_string).collect();
        return Err(not_observable(format!(
            "Check {id} needs {}, which Wokwi cannot show. check.yaml needs fixing in the curriculum.",
            kinds.join(" and ")
        )));
    }

    let watch: Vec<String> = file
        .checks
        .iter()
        .filter_map(|c| c.pin().map(str::to_string))
        .collect();
    let press = file.driven_pins();
    if (!watch.is_empty() || !press.is_empty()) && ter.pins.is_empty() {
        return Err(not_observable(format!(
            "The checks name pins, and {} has no pin map: the exercise was fetched before TER Learn sent one. Keep a copy of your src/, then `ter ex remove --force {}` and `ter ex fetch {}`.",
            dir.join(ter_sdk::project::TER_TOML).display(),
            ter.exercise,
            ter.exercise
        )));
    }
    let diagram_path = dir.join(wokwi::RUN_DIAGRAM);
    let diagram = std::fs::read_to_string(&diagram_path).map_err(|e| {
        not_observable(format!(
            "No Wokwi circuit for this exercise: could not read {} ({e}).",
            diagram_path.display()
        ))
    })?;
    let mut plan = wokwi::plan(&diagram, &ter.pins, &watch, &press).map_err(|e| {
        let checks = checks_for(file, &e);
        not_observable(format!(
            "Wokwi cannot run these checks as written: {e}{}.",
            if checks.is_empty() {
                String::new()
            } else {
                format!(" (checks: {})", checks.join(", "))
            }
        ))
    })?;
    plan.chips = chips(dir, ter)?;
    if let Some(part) = chip_parts(&plan.diagram)
        .into_iter()
        .find(|p| !plan.chips.iter().any(|c| c.name == *p))
    {
        return Err(not_observable(format!(
            "diagram.json uses the custom chip {part}, and the exercise has no {part}.chip.wasm or [[chip]] entry for it in wokwi.toml. {}",
            refetch(ter)
        )));
    }
    Ok(plan)
}

/// Getting the exercise's files back as fetched.
fn refetch(ter: &TerToml) -> String {
    format!(
        "Keep a copy of your src/, then `ter ex remove --force {0}` and `ter ex fetch {0}`.",
        ter.exercise
    )
}

/// Whether the exercise folder has a circuit for the simulator.
pub fn has_circuit(dir: &Path) -> bool {
    dir.join(wokwi::RUN_DIAGRAM).is_file()
}

/// Whether ter offers a simulation run of the exercise in `dir`: its
/// modes allow one and it has a circuit, Wokwi's or a sim86 board that
/// shows its checks. Modes that allow simulation for
/// an exercise fetched without a circuit do not send anyone to a run that
/// cannot start.
pub fn offered(dir: &Path, modes: &[String]) -> bool {
    modes.iter().any(|m| m == "simulation") && (has_circuit(dir) || crate::sim86::runs(dir))
}

/// The simulator a simulation run uses when none is named: Wokwi, or
/// sim86 for an exercise that has a sim86 board and no Wokwi circuit.
pub fn default_venue(dir: &Path) -> &'static str {
    if !has_circuit(dir) && crate::sim86::has_board(dir) {
        "sim86"
    } else {
        "wokwi"
    }
}

/// Why an exercise with no circuit does not run in simulation, and what
/// runs it instead.
fn no_circuit(ter: &TerToml) -> CliError {
    let mut message = format!(
        "{} has no Wokwi circuit ({} is not in its folder), so it cannot run in simulation.",
        ter.exercise,
        wokwi::RUN_DIAGRAM
    );
    if ter.modes.iter().any(|m| m == "hardware") {
        message.push_str(" Run it on your board: `ter run --hw`.");
    }
    not_observable(message)
}

/// The custom chip names the diagram's parts use: type `chip-<name>`.
fn chip_parts(diagram: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = diagram["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["type"].as_str()?.strip_prefix("chip-"))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The exercise's Wokwi custom chips: each `[[chip]]` in its `wokwi.toml`,
/// then every `<name>.chip.wasm` at its root that no entry names. Each
/// `.wasm` needs its `.json` beside it (`x.chip.wasm`, `x.chip.json`).
pub fn chips(dir: &Path, ter: &TerToml) -> Result<Vec<Chip>, CliError> {
    let config_path = dir.join(wokwi::RUN_CONFIG);
    let entries = match std::fs::read_to_string(&config_path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|e| {
                not_observable(format!(
                    "Could not read {}: {}",
                    config_path.display(),
                    e.message()
                ))
            })?
            .get("chip")
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let bad_entry = |why: String| {
        not_observable(format!(
            "{} has a [[chip]] entry ter cannot use: {why}.",
            config_path.display()
        ))
    };
    let mut chips: Vec<Chip> = Vec::new();
    for entry in &entries {
        let field = |key: &str| entry.get(key).and_then(toml::Value::as_str);
        let name = field("name")
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad_entry("no name".into()))?;
        let binary = field("binary").ok_or_else(|| bad_entry(format!("{name} has no binary")))?;
        let binary = ter_sdk::project::safe_relative(binary).ok_or_else(|| {
            bad_entry(format!(
                "{name}'s binary {binary:?} is not a path inside the exercise"
            ))
        })?;
        chips.push(Chip {
            name: name.to_string(),
            from: dir.join(&binary),
            binary,
        });
    }
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| not_observable(format!("Could not read {}: {e}", dir.display())))?
        .filter_map(|e| {
            let file = e.ok()?.file_name().into_string().ok()?;
            let name = file.strip_suffix(".chip.wasm")?.to_string();
            Some((name, PathBuf::from(file)))
        })
        .collect();
    found.sort();
    for (name, binary) in found {
        if !name.is_empty() && !chips.iter().any(|c| c.binary == binary || c.name == name) {
            chips.push(Chip {
                name,
                from: dir.join(&binary),
                binary,
            });
        }
    }
    for chip in &chips {
        for file in [chip.from.clone(), Chip::json_for(&chip.from)] {
            if !file.is_file() {
                return Err(not_observable(format!(
                    "The custom chip {} needs {}, which is not there. {}",
                    chip.name,
                    file.display(),
                    refetch(ter)
                )));
            }
        }
    }
    Ok(chips)
}

/// The checks a wiring problem leaves unseen.
fn checks_for(file: &CheckFile, e: &WiringError) -> Vec<String> {
    let on = |pins: &[String]| -> Vec<String> {
        file.checks
            .iter()
            .filter(|c| c.pin().is_some_and(|p| pins.iter().any(|x| x == p)))
            .map(|c| c.id.clone())
            .collect()
    };
    match e {
        WiringError::TooManyPins(pins) => on(&pins[wokwi::MAX_PINS..]),
        WiringError::NotInPinMap(pin) | WiringError::NotOnBoard { pin, .. } => {
            on(std::slice::from_ref(pin))
        }
        // A stimulus nobody can apply leaves every check unjudged.
        WiringError::NoButton { .. } => file.checks.iter().map(|c| c.id.clone()).collect(),
        WiringError::BadDiagram(_) | WiringError::NoBoard | WiringError::UnknownBoard(_) => {
            Vec::new()
        }
    }
}

/// `wokwi-cli` and the learner's token, or why Wokwi cannot be used.
pub fn ready(plan: Plan) -> Result<Ready, CliError> {
    let cli = find_cli().ok_or_else(|| {
        CliError::new(
            "venue_unavailable",
            "wokwi-cli is not installed. `ter install wokwi-cli` installs it and stores your Wokwi token.",
        )
    })?;
    let token = token()?.ok_or_else(|| {
        CliError::new(
            "venue_unavailable",
            format!(
                "No Wokwi token. Simulation runs on your own Wokwi account: create a CI token at {TOKEN_PAGE}, then `ter install wokwi-cli` stores it."
            ),
        )
    })?;
    Ok(Ready { cli, token, plan })
}

/// `WOKWI_CLI_TOKEN`, else the one `ter install wokwi-cli` stored.
pub fn token() -> Result<Option<String>, CliError> {
    if let Ok(t) = std::env::var(TOKEN_ENV)
        && !t.trim().is_empty()
    {
        return Ok(Some(t.trim().to_string()));
    }
    Ok(TokenStore::open_wokwi()?.load()?.map(|(t, _)| t))
}

/// `wokwi-cli` on the PATH, else where Wokwi's installer puts it.
pub fn find_cli() -> Option<PathBuf> {
    let name = format!("{WOKWI_CLI}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(home_bin())
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

pub fn home_bin() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(WOKWI_HOME_BIN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ter(pins: &[(&str, &str)]) -> TerToml {
        TerToml {
            exercise: "gpio-button-blink--xiao-esp32c3-nostd".into(),
            course: "c".into(),
            target: "xiao-esp32c3-nostd".into(),
            modes: vec!["hardware".into(), "simulation".into()],
            mode: "simulation".into(),
            venue: None,
            fetched_sha256: String::new(),
            pins: pins
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    const DIAGRAM: &str = r#"{"parts":[{"type":"board-xiao-esp32-c3","id":"xiao"},
        {"type":"wokwi-pushbutton","id":"btn1"}],
        "connections":[["btn1:1.l","xiao:D3","orange",[]]]}"#;

    fn exercise(diagram: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(d) = diagram {
            std::fs::write(dir.path().join("diagram.json"), d).unwrap();
        }
        dir
    }

    fn file(yaml: &str) -> CheckFile {
        CheckFile::parse(yaml).unwrap()
    }

    const PRESS: &str = "timeout_ms: 4000\nsetup:\n  - press: user_button\n    at_ms: 1000\n    hold_ms: 500\nassert:\n  - id: led-on\n    pin: user_led\n    is: high\n    after_ms: 1100\n";

    #[test]
    fn a_check_the_venue_cannot_see_is_named() {
        let f = file(PRESS);
        let serial_only: EventKinds = [Kind::Serial, Kind::Reset].into();
        assert_eq!(
            unobservable(&f, &serial_only),
            vec![("led-on".to_string(), vec![Kind::Pins, Kind::Stimulus])]
        );
        let wokwi: EventKinds = wokwi::Wokwi::PROVIDES.into();
        assert!(unobservable(&f, &wokwi).is_empty());
        // No check kind needs power, so nothing on Wokwi lacks it; a venue
        // without stimulus cannot run a setup.
        let no_stimulus: EventKinds = [Kind::Serial, Kind::Pins, Kind::Reset].into();
        assert_eq!(unobservable(&f, &no_stimulus)[0].1, vec![Kind::Stimulus]);
    }

    #[test]
    fn a_pressed_and_watched_circuit_plans() {
        let dir = exercise(Some(DIAGRAM));
        let p = plan(
            dir.path(),
            &ter(&[("user_led", "GPIO3"), ("user_button", "GPIO5")]),
            &file(PRESS),
        )
        .unwrap();
        assert_eq!(p.channels.len(), 1);
        assert_eq!(p.buttons["user_button"], "btn1");
    }

    #[test]
    fn no_pin_map_no_diagram_or_a_bad_wire_is_not_observable() {
        let dir = exercise(Some(DIAGRAM));
        let err = plan(dir.path(), &ter(&[]), &file(PRESS)).unwrap_err();
        assert_eq!(err.code, "not_observable");
        assert!(err.message.contains("no pin map"), "{}", err.message);

        let bare = exercise(None);
        let err = plan(
            bare.path(),
            &ter(&[("user_led", "GPIO3"), ("user_button", "GPIO5")]),
            &file(PRESS),
        )
        .unwrap_err();
        assert!(
            err.message.contains("has no Wokwi circuit"),
            "{}",
            err.message
        );
        assert!(err.message.contains("ter run --hw"), "{}", err.message);
        assert!(!offered(bare.path(), &ter(&[]).modes));
        assert!(offered(dir.path(), &ter(&[]).modes));
        assert!(!offered(dir.path(), &["hardware".to_string()]));

        let err = plan(
            dir.path(),
            &ter(&[("user_led", "GPIO3"), ("user_button", "GPIO4")]),
            &file(PRESS),
        )
        .unwrap_err();
        assert_eq!(err.code, "not_observable");
        assert!(
            err.message.contains("no pushbutton") && err.message.contains("led-on"),
            "{}",
            err.message
        );
    }

    #[test]
    fn nine_pins_are_refused_naming_the_checks_past_eight() {
        let gpios = [
            "GPIO2", "GPIO3", "GPIO4", "GPIO5", "GPIO6", "GPIO7", "GPIO21", "GPIO20", "GPIO8",
        ];
        let pins: Vec<(String, String)> = gpios
            .iter()
            .enumerate()
            .map(|(i, g)| (format!("p{i}"), g.to_string()))
            .collect();
        let pin_refs: Vec<(&str, &str)> =
            pins.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let checks: String = (0..9)
            .map(|i| format!("  - id: c{i}\n    pin: p{i}\n    is: high\n    after_ms: 0\n"))
            .collect();
        let dir = exercise(Some(DIAGRAM));
        let err = plan(
            dir.path(),
            &ter(&pin_refs),
            &file(&format!("timeout_ms: 1000\nassert:\n{checks}")),
        )
        .unwrap_err();
        assert_eq!(err.code, "not_observable");
        assert!(err.message.contains("9 pins"), "{}", err.message);
        assert!(err.message.contains("(checks: c8)"), "{}", err.message);
    }

    #[test]
    fn serial_only_checks_need_no_pin_map() {
        let dir = exercise(Some(DIAGRAM));
        let p = plan(
            dir.path(),
            &ter(&[]),
            &file("timeout_ms: 1000\nassert:\n  - id: hi\n    serial_contains: hi\n    within_ms: 500\n"),
        )
        .unwrap();
        assert!(p.channels.is_empty());
    }

    const ALARM: &str = r#"{"parts":[{"type":"board-xiao-esp32-c3","id":"xiao"},
        {"type":"chip-tca6424","id":"io1"}],"connections":[]}"#;
    const SERIAL: &str =
        "timeout_ms: 1000\nassert:\n  - id: hi\n    serial_contains: hi\n    within_ms: 500\n";

    /// A fake chip: a `.wasm` of every byte value and its `.json`.
    fn fake_chip(dir: &Path, wasm: &str) {
        let wasm = dir.join(wasm);
        std::fs::create_dir_all(wasm.parent().unwrap()).unwrap();
        std::fs::write(&wasm, (0..=255u8).collect::<Vec<_>>()).unwrap();
        std::fs::write(wasm.with_extension("json"), "{}").unwrap();
    }

    #[test]
    fn root_chip_files_and_wokwi_toml_entries_are_the_run_chips() {
        let dir = exercise(Some(ALARM));
        fake_chip(dir.path(), "tca6424.chip.wasm");
        fake_chip(dir.path(), "extra.chip.wasm");
        fake_chip(dir.path(), "dist/chip.wasm");
        std::fs::write(
            dir.path().join("wokwi.toml"),
            "[wokwi]\nversion = 1\nfirmware = \"target/x\"\n\n[[chip]]\nname = \"tca6424\"\nbinary = \"tca6424.chip.wasm\"\n\n[[chip]]\nname = \"nested\"\nbinary = \"dist/chip.wasm\"\n",
        )
        .unwrap();
        let p = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap();
        let got: Vec<(&str, PathBuf)> = p
            .chips
            .iter()
            .map(|c| (c.name.as_str(), c.binary.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("tca6424", PathBuf::from("tca6424.chip.wasm")),
                ("nested", Path::new("dist").join("chip.wasm")),
                // At the root with no entry: named after the file.
                ("extra", PathBuf::from("extra.chip.wasm")),
            ]
        );
        assert_eq!(p.chips[0].from, dir.path().join("tca6424.chip.wasm"));
    }

    #[test]
    fn a_root_chip_needs_no_wokwi_toml() {
        let dir = exercise(Some(ALARM));
        fake_chip(dir.path(), "tca6424.chip.wasm");
        let p = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap();
        assert_eq!(p.chips.len(), 1);
        assert_eq!(p.chips[0].name, "tca6424");
    }

    #[test]
    fn a_missing_or_unsafe_chip_is_not_observable() {
        // The diagram uses a chip the exercise does not have.
        let dir = exercise(Some(ALARM));
        let err = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap_err();
        assert_eq!(err.code, "not_observable");
        assert!(
            err.message.contains("custom chip tca6424"),
            "{}",
            err.message
        );

        // The .wasm without its .json.
        fake_chip(dir.path(), "tca6424.chip.wasm");
        std::fs::remove_file(dir.path().join("tca6424.chip.json")).unwrap();
        let err = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap_err();
        assert!(err.message.contains("tca6424.chip.json"), "{}", err.message);
        assert!(err.message.contains("ter ex fetch"), "{}", err.message);

        // A [[chip]] binary outside the exercise.
        fake_chip(dir.path(), "tca6424.chip.wasm");
        std::fs::write(
            dir.path().join("wokwi.toml"),
            "[[chip]]\nname = \"tca6424\"\nbinary = \"../tca6424.chip.wasm\"\n",
        )
        .unwrap();
        let err = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap_err();
        assert!(err.message.contains("not a path inside"), "{}", err.message);
    }

    #[test]
    fn no_chips_no_chip_files() {
        let dir = exercise(Some(DIAGRAM));
        std::fs::write(dir.path().join("wokwi.toml"), "[wokwi]\nversion = 1\n").unwrap();
        let p = plan(dir.path(), &ter(&[]), &file(SERIAL)).unwrap();
        assert!(p.chips.is_empty());
    }
}
