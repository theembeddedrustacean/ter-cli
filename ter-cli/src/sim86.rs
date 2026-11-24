//! sim86 as the simulation venue of `ter run --sim sim86`: where ter keeps
//! the sim86 package, whether an exercise's board can show its checks, and
//! whether the learner's sim86 account can run it.
//!
//! As with Wokwi, everything that can stop the run is settled before the
//! build: a check the board cannot show is `not_observable`, and a missing
//! install, a missing or refused key and an empty account are
//! `venue_unavailable`, with nothing posted.

use std::path::{Path, PathBuf};

use ter_check::CheckFile;
use ter_flash::Tool;
use ter_sdk::project::TerToml;
use ter_telemetry::sim86::{
    self, BOARD_FILE, Balance, Failure, FailureKind, HELPER, HELPER_FILE, Helper, Image, KEY_ENV,
    PINS_FILE, Plan, WiringError,
};

use crate::output::CliError;

/// The folder ter keeps its own tools in, instead of the platform's.
pub const TOOLS_ENV: &str = "TER_TOOLS_DIR";

/// A simulated run on sim86, ready to go once the program is built.
pub struct Ready {
    pub helper: Helper,
    pub board: PathBuf,
    pub plan: Plan,
    pub image: Image,
}

/// Where `ter install sim86` puts the Python environment and the helper.
pub fn home() -> Result<PathBuf, CliError> {
    if let Some(dir) = std::env::var_os(TOOLS_ENV).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("sim86"));
    }
    directories::ProjectDirs::from("com", "The Embedded Rustacean", "ter")
        .map(|d| d.data_local_dir().join("tools").join("sim86"))
        .ok_or_else(|| CliError::new("config_error", "Could not find a home directory."))
}

/// The Python of the environment in `home`, and the sim86 command beside
/// it.
pub fn python_in(home: &Path) -> PathBuf {
    let venv = home.join("venv");
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

pub fn command_in(home: &Path) -> PathBuf {
    python_in(home).with_file_name(format!("sim86{}", std::env::consts::EXE_SUFFIX))
}

/// The learner's key when it is in the environment. Otherwise the sim86
/// package finds the one `sim86 login` stored; ter never reads that.
fn key() -> Option<String> {
    std::env::var(KEY_ENV)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// The installed helper, or `None` before `ter install sim86`.
pub fn installed() -> Option<Helper> {
    let home = home().ok()?;
    let python = python_in(&home);
    python.is_file().then(|| Helper {
        python,
        script: home.join(HELPER_FILE),
        key: key(),
    })
}

/// Write the helper this ter ships, unless that is what is there.
pub fn write_helper(helper: &Helper) -> std::io::Result<()> {
    if std::fs::read_to_string(&helper.script).is_ok_and(|have| have == HELPER) {
        return Ok(());
    }
    std::fs::write(&helper.script, HELPER)
}

/// Whether the folder has a sim86 board and its pin map.
pub fn has_board(dir: &Path) -> bool {
    dir.join(BOARD_FILE).is_file() && dir.join(PINS_FILE).is_file()
}

/// Find the nets and buttons `file`'s checks need on the board whose two
/// files are given as text.
pub fn wiring(board: &str, pins: &str, file: &CheckFile) -> Result<Plan, WiringError> {
    let watch: Vec<String> = file
        .checks
        .iter()
        .filter_map(|c| c.pin().map(str::to_string))
        .collect();
    sim86::plan(board, pins, &watch, &file.driven_pins())
}

/// Whether sim86 can run `file` on the folder's board: `None` when the
/// folder has no sim86 board, else the plan or why not.
pub fn fits(dir: &Path, file: &CheckFile) -> Option<Result<Plan, String>> {
    if !has_board(dir) {
        return None;
    }
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name)).map_err(|e| format!("could not read {name}: {e}"))
    };
    Some(
        read(BOARD_FILE)
            .and_then(|board| Ok((board, read(PINS_FILE)?)))
            .and_then(|(board, pins)| wiring(&board, &pins, file).map_err(|e| e.to_string())),
    )
}

/// Whether sim86 can run the exercise in `dir` as its `check.yaml` stands.
pub fn runs(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(crate::run::CHECK_YAML))
        .ok()
        .and_then(|text| CheckFile::parse(&text).ok())
        .and_then(|file| fits(dir, &file))
        .is_some_and(|plan| plan.is_ok())
}

/// The plan for a run, or `not_observable`.
pub fn plan(dir: &Path, ter: &TerToml, file: &CheckFile) -> Result<Plan, CliError> {
    let not_observable = |m: String| CliError::new("not_observable", m);
    match fits(dir, file) {
        None => {
            let mut message = format!(
                "{} has no sim86 board ({BOARD_FILE} and {PINS_FILE} are not both in its folder), so it cannot run on sim86.",
                ter.exercise
            );
            if crate::sim::has_circuit(dir) {
                message.push_str(" Run it in Wokwi: `ter run --sim`.");
            } else if ter.modes.iter().any(|m| m == "hardware") {
                message.push_str(" Run it on your board: `ter run --hw`.");
            }
            Err(not_observable(message))
        }
        Some(Err(why)) => Err(not_observable(format!(
            "sim86 cannot run these checks as written: {why}."
        ))),
        Some(Ok(plan)) => Ok(plan),
    }
}

fn unavailable(message: impl Into<String>) -> CliError {
    CliError::new("venue_unavailable", message)
}

/// The form sim86 takes this exercise's program in, from the tool its
/// runner flashes with: an ESP chip's merged flash image, else the ELF.
pub fn image(dir: &Path) -> Result<Image, CliError> {
    match crate::hw::tool(dir) {
        Ok(Tool::Espflash { chip }) => {
            let chip = chip.ok_or_else(|| {
                unavailable(
                    "The exercise's runner does not name the chip (`--chip`), and sim86 needs the flash image made for it.",
                )
            })?;
            let espflash = ter_flash::find_program("espflash").ok_or_else(|| {
                unavailable(
                    "espflash is not installed, and it makes the flash image sim86 runs for this chip. Install it with `ter install espflash`.",
                )
            })?;
            Ok(Image::EspMerged { espflash, chip })
        }
        _ => Ok(Image::Elf),
    }
}

/// What the account's state means for a run about to start: `Err` stops
/// it, `Ok` lets it go, with the balance when sim86 gave one.
pub fn account(answer: Result<Balance, Failure>) -> Result<Option<Balance>, CliError> {
    match answer {
        Ok(balance) if balance.credits <= 0 => Err(unavailable(format!(
            "{}.",
            Failure {
                kind: FailureKind::Credits,
                message: format!("{} credits, plan {}", balance.credits, balance.plan),
            }
            .explain()
        ))),
        Ok(balance) => Ok(Some(balance)),
        Err(failure)
            if matches!(
                failure.kind,
                FailureKind::NoKey | FailureKind::Auth | FailureKind::Missing
            ) =>
        {
            Err(unavailable(format!("{}.", failure.explain())))
        }
        // The balance is asked through a call sim86 does not document. If
        // it stops answering, the run still goes ahead.
        Err(_) => Ok(None),
    }
}

/// The installed package with this ter's helper beside it, or why sim86
/// cannot be used on this machine.
fn helper() -> Result<Helper, CliError> {
    let helper = installed().ok_or_else(|| {
        let (major, minor) = crate::install::MIN_PYTHON;
        unavailable(format!(
            "sim86 is not installed. `ter install sim86` installs it, and needs Python {major}.{minor} or newer on this machine; it runs on your own sim86 account."
        ))
    })?;
    write_helper(&helper).map_err(|e| {
        CliError::new(
            "io_error",
            format!("Could not write {}: {e}", helper.script.display()),
        )
    })?;
    Ok(helper)
}

/// What this machine needs before any sim86 run: the package and a key.
/// Asks sim86 nothing, so a key that is there may still be refused, and
/// the account may have no credits; a run finds that out.
pub fn setup() -> Result<(), CliError> {
    let helper = helper()?;
    if helper.key.is_some() {
        return Ok(());
    }
    match helper.version() {
        Ok(found) if found.key == Some(true) => Ok(()),
        Ok(_) => Err(unavailable(format!(
            "{}.",
            Failure {
                kind: FailureKind::NoKey,
                message: String::new(),
            }
            .explain()
        ))),
        Err(failure) => Err(unavailable(format!("{}.", failure.explain()))),
    }
}

/// The installed package, the program's form and the account, or why
/// sim86 cannot be used.
pub fn ready(dir: &Path, plan: Plan) -> Result<Ready, CliError> {
    let helper = helper()?;
    let image = image(dir)?;
    account(helper.balance())?;
    Ok(Ready {
        helper,
        board: dir.join(BOARD_FILE),
        plan,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = r#"{"parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"},
        {"id": "led1", "type": "Led"}, {"id": "btn1", "type": "Btn"}],
        "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"], ["btn1.L1", "mcu.D3"]]}"#;
    const PINS: &str =
        r#"{"user_led": "mcu.D1", "buzzer": "mcu.D2", "user_button": {"part": "btn1"}}"#;
    const LED: &str = "timeout_ms: 4000\nsetup:\n  - press: user_button\n    at_ms: 1000\n    hold_ms: 500\nassert:\n  - id: led-on\n    pin: user_led\n    is: high\n    after_ms: 1100\n";
    const BUZZER: &str = "timeout_ms: 1000\nassert:\n  - id: hi\n    serial_contains: hi\n    within_ms: 500\n  - id: tone\n    pin: buzzer\n    toggles_per_s: { min: 1, max: 9 }\n    window_ms: [0, 900]\n";

    fn ter(modes: &[&str]) -> TerToml {
        TerToml {
            exercise: "gpio-x".into(),
            course: "c".into(),
            target: "xiao-esp32c3-nostd".into(),
            modes: modes.iter().map(|m| m.to_string()).collect(),
            mode: "simulation".into(),
            venue: None,
            fetched_sha256: String::new(),
            pins: Default::default(),
        }
    }

    fn exercise(board: Option<&str>, pins: Option<&str>, check: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sim86")).unwrap();
        if let Some(board) = board {
            std::fs::write(dir.path().join(BOARD_FILE), board).unwrap();
        }
        if let Some(pins) = pins {
            std::fs::write(dir.path().join(PINS_FILE), pins).unwrap();
        }
        std::fs::write(dir.path().join("check.yaml"), check).unwrap();
        dir
    }

    fn file(yaml: &str) -> CheckFile {
        CheckFile::parse(yaml).unwrap()
    }

    #[test]
    fn a_board_that_shows_every_check_runs() {
        let dir = exercise(Some(BOARD), Some(PINS), LED);
        assert!(has_board(dir.path()) && runs(dir.path()));
        let p = plan(dir.path(), &ter(&["simulation"]), &file(LED)).unwrap();
        assert_eq!(p.watch[0].net, "mcu.D1");
        assert!(p.buttons.contains_key("user_button"));
    }

    #[test]
    fn a_folder_without_both_files_is_not_run_on_sim86() {
        for (board, pins) in [(None, None), (Some(BOARD), None), (None, Some(PINS))] {
            let dir = exercise(board, pins, LED);
            assert!(!has_board(dir.path()) && !runs(dir.path()));
            assert!(fits(dir.path(), &file(LED)).is_none());
            let err = plan(dir.path(), &ter(&["hardware", "simulation"]), &file(LED)).unwrap_err();
            assert_eq!(err.code, "not_observable");
            assert!(
                err.message.contains("has no sim86 board"),
                "{}",
                err.message
            );
            assert!(err.message.contains("ter run --hw"), "{}", err.message);
        }
        // With a Wokwi circuit beside it, that is what runs it.
        let dir = exercise(None, None, LED);
        std::fs::write(dir.path().join("diagram.json"), "{}").unwrap();
        let err = plan(dir.path(), &ter(&["hardware", "simulation"]), &file(LED)).unwrap_err();
        assert!(err.message.contains("`ter run --sim`."), "{}", err.message);
    }

    #[test]
    fn a_check_on_a_pin_with_no_wire_is_not_run_on_sim86() {
        let dir = exercise(Some(BOARD), Some(PINS), BUZZER);
        assert!(has_board(dir.path()));
        assert!(!runs(dir.path()));
        let err = plan(dir.path(), &ter(&["simulation"]), &file(BUZZER)).unwrap_err();
        assert_eq!(err.code, "not_observable");
        assert!(
            err.message.contains("buzzer is mcu.D2") && err.message.contains("no wire"),
            "{}",
            err.message
        );
        assert_eq!(
            wiring(BOARD, PINS, &file(BUZZER)).unwrap_err().pin(),
            Some("buzzer")
        );
    }

    #[test]
    fn an_empty_account_a_missing_key_and_a_refused_one_stop_the_run() {
        let balance = |credits| Balance {
            credits,
            plan: "free".into(),
        };
        assert_eq!(account(Ok(balance(12))).unwrap(), Some(balance(12)));
        let err = account(Ok(balance(0))).unwrap_err();
        assert_eq!(err.code, "venue_unavailable");
        assert!(err.message.contains("no credits left"), "{}", err.message);

        let failed = |kind| {
            account(Err(Failure {
                kind,
                message: "said".into(),
            }))
        };
        for kind in [FailureKind::NoKey, FailureKind::Auth, FailureKind::Missing] {
            assert_eq!(failed(kind).unwrap_err().code, "venue_unavailable");
        }
        assert!(
            failed(FailureKind::NoKey)
                .unwrap_err()
                .message
                .contains("sim86 login")
        );
        // sim86 not saying the balance does not stop a run.
        for kind in [FailureKind::Api, FailureKind::Helper, FailureKind::Busy] {
            assert_eq!(failed(kind).unwrap(), None);
        }
    }

    #[test]
    fn the_tools_folder_holds_the_environment_and_the_helper() {
        let home = Path::new("tools").join("sim86");
        let python = python_in(&home);
        assert!(python.starts_with(home.join("venv")));
        assert_eq!(
            command_in(&home).file_stem().unwrap().to_str().unwrap(),
            "sim86"
        );
    }
}
