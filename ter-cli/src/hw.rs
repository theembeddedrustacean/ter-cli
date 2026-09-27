//! The local venue for `ter run --hw`: the learner's board on USB.
//!
//! Everything that can stop a run on the board is settled here, before the
//! build: which tool flashes it (the scaffold's runner: the target's
//! `flash.tool`), whether that tool is installed, and which port the board
//! is on. Any of those missing is `venue_unavailable` with nothing posted.
//! What the board cannot see is not refused: those checks are named as
//! unseen after the run (section 6.2 of the design).

use std::path::{Path, PathBuf};

use ter_check::CheckFile;
use ter_flash::{Port, Tool};
use ter_telemetry::EventKinds;
use ter_telemetry::local::Local;

use crate::output::CliError;
use crate::sim::unobservable;

/// Names the board's port when ter should not pick one itself.
pub const PORT_ENV: &str = "TER_PORT";

/// A run on the board, ready to go once the program is built.
pub struct Ready {
    pub tool: Tool,
    pub program: PathBuf,
    pub port: Port,
}

impl Ready {
    pub fn provides(&self) -> EventKinds {
        Local::provides_on(&self.tool, self.port.kind)
    }
}

fn unavailable(message: impl Into<String>) -> CliError {
    CliError::new("venue_unavailable", message)
}

/// The flashing tool the exercise's runner names.
pub fn tool(dir: &Path) -> Result<Tool, CliError> {
    let config = dir.join(".cargo/config.toml");
    let runner = std::fs::read_to_string(&config)
        .ok()
        .and_then(|t| ter_sdk::project::find_runner(&t))
        .ok_or_else(|| {
            unavailable(format!(
                "{} sets no runner, so ter does not know how to flash this board.",
                config.display()
            ))
        })?;
    Tool::from_runner(&runner)
        .map_err(|e| unavailable(format!("ter cannot flash this board: {e}.")))
}

/// The tool, its program and the board's port, or why the board cannot be
/// used.
pub fn ready(dir: &Path) -> Result<Ready, CliError> {
    let tool = tool(dir)?;
    let program = ter_flash::find_program(tool.program()).ok_or_else(|| {
        unavailable(format!(
            "{0} is not installed, and this board is flashed with it. Install it with `ter install {0}`.",
            tool.program()
        ))
    })?;
    let port = port(&tool)?;
    Ok(Ready {
        tool,
        program,
        port,
    })
}

/// `TER_PORT`, else the one board on USB this tool can flash.
pub fn port(tool: &Tool) -> Result<Port, CliError> {
    if let Ok(path) = std::env::var(PORT_ENV)
        && !path.trim().is_empty()
    {
        let path = path.trim();
        if !ter_flash::port::present(path) {
            return Err(unavailable(format!(
                "{PORT_ENV} is {path}, and there is no such port. Is the board plugged in?"
            )));
        }
        return Ok(Port::named(path));
    }
    let boards: Vec<Port> = ter_flash::port::list()
        .into_iter()
        .filter(|p| tool.wants(p.kind))
        .collect();
    match boards.len() {
        0 => Err(unavailable(format!(
            "No board found on USB for {}. Plug it in with a data cable (`ter venues` lists what ter sees), or name its port with {PORT_ENV}.",
            tool.program()
        ))),
        1 => Ok(boards.into_iter().next().expect("one board")),
        _ => Err(unavailable(format!(
            "{} boards are plugged in ({}). Set {PORT_ENV} to the one to use.",
            boards.len(),
            boards
                .iter()
                .map(|p| p.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The checks the board cannot see, as one line for before the build, or
/// `None` when it sees them all.
pub fn unseen_note(file: &CheckFile, provides: &EventKinds) -> Option<String> {
    let unseen: Vec<String> = unobservable(file, provides)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    (!unseen.is_empty()).then(|| {
        format!(
            "A bare board shows serial only: {} of {} checks cannot be seen here ({}).",
            unseen.len(),
            file.checks.len(),
            unseen.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ter_flash::PortKind;

    #[test]
    fn the_tool_comes_from_the_exercises_runner() {
        let dir = tempfile::tempdir().unwrap();
        let err = tool(dir.path()).unwrap_err();
        assert_eq!(err.code, "venue_unavailable");
        std::fs::create_dir(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n",
        )
        .unwrap();
        assert_eq!(
            tool(dir.path()).unwrap(),
            Tool::Espflash {
                chip: Some("esp32c3".into())
            }
        );
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[target.thumbv6m-none-eabi]\nrunner = \"elf2uf2-rs -d\"\n",
        )
        .unwrap();
        let err = tool(dir.path()).unwrap_err();
        assert!(err.message.contains("elf2uf2-rs"), "{}", err.message);
    }

    #[test]
    fn a_bare_board_names_the_checks_it_cannot_see() {
        let file = CheckFile::parse(
            "timeout_ms: 3000\nassert:\n  - id: banner\n    serial_contains: hi\n    within_ms: 2000\n  - id: blink\n    pin: user_led\n    toggles_per_s: { min: 1.8, max: 2.2 }\n    window_ms: [600, 2600]\n",
        )
        .unwrap();
        let tool = Tool::Espflash { chip: None };
        let provides = Local::provides_on(&tool, PortKind::UsbSerialJtag);
        assert_eq!(
            unseen_note(&file, &provides).unwrap(),
            "A bare board shows serial only: 1 of 2 checks cannot be seen here (blink)."
        );
        let serial_only = CheckFile::parse(
            "timeout_ms: 3000\nassert:\n  - id: banner\n    serial_contains: hi\n    within_ms: 2000\n",
        )
        .unwrap();
        assert_eq!(unseen_note(&serial_only, &provides), None);
    }
}
