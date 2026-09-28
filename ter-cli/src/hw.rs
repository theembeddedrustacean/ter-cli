//! The local venue for `ter run --hw`: the learner's board on USB.
//!
//! Everything that can stop a run on the board is settled here, before the
//! build: which tool flashes it (the scaffold's runner: the target's
//! `flash.tool`), whether that tool is installed, and which port the board
//! is on. Any of those missing is `venue_unavailable` with nothing posted.
//! What the board cannot see is not refused: those checks are named as
//! unseen after the run (section 6.2 of the design).
//!
//! A UF2 board is flashed through its bootloader's drive, so what has to
//! be there before the build is the drive: the board in its bootloader.
//! On a terminal ter asks for it and waits; otherwise it says how.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ter_check::CheckFile;
use ter_flash::drive::Drive;
use ter_flash::{Family, Port, Tool};
use ter_telemetry::EventKinds;
use ter_telemetry::local::Local;

use crate::output::CliError;
use crate::sim::unobservable;

/// Names the board's port when ter should not pick one itself.
pub const PORT_ENV: &str = "TER_PORT";
/// Names a UF2 board's drive (its mount point) when ter should not look
/// for one itself.
pub const UF2_DRIVE_ENV: &str = "TER_UF2_DRIVE";
/// How long ter waits, on a terminal, for a UF2 board to enter its
/// bootloader.
const BOOTLOADER_WAIT: Duration = Duration::from_secs(120);

/// A run on the board, ready to go once the program is built.
pub struct Ready {
    pub tool: Tool,
    /// The tool's program; empty for a UF2 board.
    pub program: PathBuf,
    /// Empty for a UF2 board unless named: its program's port appears
    /// after the flash.
    pub port: Port,
    /// A UF2 board's bootloader drive.
    pub drive: Option<Drive>,
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
/// used. `ask`: a UF2 board not in its bootloader is asked for on the
/// terminal, and waited for.
pub fn ready(dir: &Path, ask: bool) -> Result<Ready, CliError> {
    let tool = tool(dir)?;
    if let Tool::Uf2 { family } = tool {
        let drive = uf2_drive(family, ask && std::io::stderr().is_terminal())?;
        let port = named_port().unwrap_or_default();
        return Ok(Ready {
            tool,
            program: PathBuf::new(),
            port,
            drive: Some(drive),
        });
    }
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
        drive: None,
    })
}

/// `TER_PORT`, as a port, when it is set. A UF2 board's program port is
/// not there yet while the board is in its bootloader.
fn named_port() -> Option<Port> {
    std::env::var(PORT_ENV)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(|path| Port {
            path,
            ..Port::default()
        })
}

/// `TER_PORT`, else the one board on USB this tool can flash.
pub fn port(tool: &Tool) -> Result<Port, CliError> {
    if let Some(named) = named_port() {
        let path = named.path.as_str();
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

/// The UF2 drive of a `family` board in its bootloader: `TER_UF2_DRIVE`,
/// else the one mounted. With `wait`, ter asks for it and waits.
pub fn uf2_drive(family: &'static Family, wait: bool) -> Result<Drive, CliError> {
    if let Ok(path) = std::env::var(UF2_DRIVE_ENV)
        && !path.trim().is_empty()
    {
        let path = path.trim();
        let drive = Drive::at(Path::new(path)).ok_or_else(|| {
            unavailable(format!(
                "{UF2_DRIVE_ENV} is {path}, and there is no UF2 drive there (no {}). Put the board in its bootloader: {}.",
                ter_flash::drive::INFO_FILE,
                family.bootloader
            ))
        })?;
        return fits(drive, family);
    }
    let started = Instant::now();
    let mut asked = false;
    let mut told: Vec<PathBuf> = Vec::new();
    loop {
        let (right, wrong): (Vec<Drive>, Vec<Drive>) = ter_flash::drive::mounted()
            .into_iter()
            .partition(|d| d.family().is_none_or(|f| f == family));
        match right.len() {
            1 => return Ok(right.into_iter().next().expect("one drive")),
            0 => {}
            n => {
                return Err(unavailable(format!(
                    "{n} UF2 drives are mounted ({}). Set {UF2_DRIVE_ENV} to the board's.",
                    right
                        .iter()
                        .map(|d| d.path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        let unmounted = ter_flash::drive::unmounted();
        if !wait || started.elapsed() > BOOTLOADER_WAIT {
            return Err(unavailable(no_drive(family, &wrong, &unmounted)));
        }
        if !asked {
            eprintln!(
                "Put the {} board in its bootloader: {}. Waiting for its UF2 drive (Ctrl-C to stop).",
                family.chip, family.bootloader
            );
            asked = true;
        }
        for disk in unmounted {
            if !told.contains(&disk) {
                eprintln!("{}", mount_it(&disk));
                told.push(disk);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn fits(drive: Drive, family: &Family) -> Result<Drive, CliError> {
    match drive.family() {
        Some(other) if other != family => Err(unavailable(format!(
            "The UF2 drive {} is an {}'s ({}), and this exercise is for the {}.",
            drive.path.display(),
            other.chip,
            drive.board(),
            family.chip
        ))),
        _ => Ok(drive),
    }
}

/// Why there is no drive to flash, and what to do.
fn no_drive(family: &Family, wrong: &[Drive], unmounted: &[PathBuf]) -> String {
    let mut message = format!(
        "No UF2 drive for the {} is mounted. Put the board in its bootloader ({}), then run again; or name the drive with {UF2_DRIVE_ENV}.",
        family.chip, family.bootloader
    );
    for d in wrong {
        message.push_str(&format!(
            " The UF2 drive {} is {}, another board.",
            d.path.display(),
            d.board()
        ));
    }
    for disk in unmounted {
        message.push(' ');
        message.push_str(&mount_it(disk));
    }
    message
}

fn mount_it(disk: &Path) -> String {
    format!(
        "A UF2 bootloader is on USB at {0} but not mounted: mount it (for example `udisksctl mount -b {0}`).",
        disk.display()
    )
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
        assert_eq!(
            tool(dir.path()).unwrap(),
            Tool::Uf2 {
                family: &ter_flash::uf2::RP2040
            }
        );
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[target.thumbv6m-none-eabi]\nrunner = \"hf2 elf\"\n",
        )
        .unwrap();
        let err = tool(dir.path()).unwrap_err();
        assert!(err.message.contains("hf2"), "{}", err.message);
    }

    #[test]
    fn no_drive_says_how_to_get_one() {
        let rp = Drive {
            path: "/media/omar/RPI-RP2".into(),
            info: "Model: Raspberry Pi RP2\nBoard-ID: RPI-RP2\n".into(),
            device: None,
        };
        let m = no_drive(
            &ter_flash::uf2::NRF52840,
            std::slice::from_ref(&rp),
            &[PathBuf::from("/dev/sdc")],
        );
        assert!(
            m.starts_with("No UF2 drive for the nRF52840 is mounted."),
            "{m}"
        );
        assert!(m.contains("double-tap RESET"), "{m}");
        assert!(
            m.contains("/media/omar/RPI-RP2 is Raspberry Pi RP2 (RPI-RP2), another board"),
            "{m}"
        );
        assert!(m.contains("`udisksctl mount -b /dev/sdc`"), "{m}");
        let err = fits(rp.clone(), &ter_flash::uf2::NRF52840).unwrap_err();
        assert!(err.message.contains("is an RP2040's"), "{}", err.message);
        assert_eq!(fits(rp.clone(), &ter_flash::uf2::RP2040).unwrap(), rp);
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
