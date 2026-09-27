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
use ter_telemetry::wokwi::{self, Plan, WiringError};
use ter_telemetry::{EventKinds, Kind};

use crate::output::CliError;
use crate::token_store::TokenStore;

pub const WOKWI_CLI: &str = "wokwi-cli";
/// Where Wokwi's own installer puts `wokwi-cli`, under the home folder.
pub const WOKWI_HOME_BIN: &str = ".wokwi/bin";
pub const TOKEN_ENV: &str = "WOKWI_CLI_TOKEN";
pub const TOKEN_PAGE: &str = "https://wokwi.com/dashboard/ci";

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
    wokwi::plan(&diagram, &ter.pins, &watch, &press).map_err(|e| {
        let checks = checks_for(file, &e);
        not_observable(format!(
            "Wokwi cannot run these checks as written: {e}{}.",
            if checks.is_empty() {
                String::new()
            } else {
                format!(" (checks: {})", checks.join(", "))
            }
        ))
    })
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
        assert!(err.message.contains("No Wokwi circuit"), "{}", err.message);

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
}
