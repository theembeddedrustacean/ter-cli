//! Flashing and resetting boards with espflash or probe-rs, or through a
//! UF2 bootloader's drive.
//!
//! Which tool flashes a board is the target's `flash.tool`. The scaffold
//! carries it as its `cargo run` runner (`espflash flash --monitor --chip
//! esp32c3`, `probe-rs run --chip STM32F401RETx`, `elf2uf2-rs -d`), so
//! [`Tool::from_runner`] reads it there: the same tool, and the same chip,
//! that free play uses. A UF2 runner only tells ter the chip family: ter
//! makes and copies the UF2 file itself.
//!
//! This crate must never depend on anything that reaches the network; the
//! local check enforces it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub mod drive;
pub mod port;
pub mod uf2;

pub use port::{Port, PortKind};
pub use uf2::Family;

/// A flashing tool and the chip it is told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// `chip` is `None` when the runner lets espflash detect it.
    Espflash {
        chip: Option<String>,
    },
    ProbeRs {
        chip: String,
    },
    /// Copied to the board's UF2 drive, by ter itself.
    Uf2 {
        family: &'static Family,
    },
}

impl Tool {
    /// The tool a scaffold's runner uses, or why ter cannot use it.
    pub fn from_runner(runner: &str) -> Result<Self, String> {
        let words: Vec<&str> = runner.split_whitespace().collect();
        let program = words.first().copied().unwrap_or_default();
        let name = Path::new(program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let chip = chip_arg(&words);
        match name {
            "espflash" => Ok(Tool::Espflash { chip }),
            "probe-rs" => chip.map(|chip| Tool::ProbeRs { chip }).ok_or_else(|| {
                format!("the runner `{runner}` does not name the chip for probe-rs")
            }),
            // elf2uf2-rs makes RP2040 files only.
            "elf2uf2-rs" => Ok(Tool::Uf2 {
                family: &uf2::RP2040,
            }),
            "uf2deploy" => {
                let family = arg(&words, "--family", "-f")
                    .ok_or_else(|| format!("the runner `{runner}` does not name the UF2 family"))?;
                Family::named(&family)
                    .map(|family| Tool::Uf2 { family })
                    .ok_or_else(|| {
                        format!(
                            "ter knows the UF2 families {}, and the runner `{runner}` names {family}",
                            uf2::FAMILIES.map(|f| f.name).join(", ")
                        )
                    })
            }
            _ => Err(format!(
                "ter flashes with espflash, probe-rs or a UF2 runner (elf2uf2-rs, uf2deploy), and the runner is `{runner}`"
            )),
        }
    }

    /// The program's name, as it is found on the PATH: `uf2` for a UF2
    /// board, which ter flashes without one.
    pub fn program(&self) -> &'static str {
        match self {
            Tool::Espflash { .. } => "espflash",
            Tool::ProbeRs { .. } => "probe-rs",
            Tool::Uf2 { .. } => "uf2",
        }
    }

    /// Whether flashing runs another program, which must be installed.
    pub fn external(&self) -> bool {
        !matches!(self, Tool::Uf2 { .. })
    }

    /// What a board this tool flashes shows up as on USB.
    pub fn wants(&self, kind: PortKind) -> bool {
        match self {
            Tool::Espflash { .. } => matches!(kind, PortKind::UsbSerialJtag | PortKind::UsbUart),
            Tool::ProbeRs { .. } => kind == PortKind::Probe,
            // Flashed through its drive; the port comes after.
            Tool::Uf2 { .. } => false,
        }
    }

    /// Arguments that write `elf` to the board. With `hold`, an ESP board
    /// is left in its bootloader for the caller to reset (and so see its
    /// first byte); otherwise the tool starts the program itself. None for
    /// a UF2 board: see [`drive::Drive::copy`].
    pub fn flash_args(&self, elf: &Path, port: Option<&str>, hold: bool) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut push = |s: &str| args.push(s.into());
        match self {
            Tool::Espflash { chip } => {
                push("flash");
                push("--non-interactive");
                if let Some(chip) = chip {
                    push("--chip");
                    push(chip);
                }
                if let Some(port) = port {
                    push("--port");
                    push(port);
                }
                push("--after");
                push(if hold { "no-reset" } else { "hard-reset" });
            }
            Tool::ProbeRs { chip } => {
                push("download");
                push("--non-interactive");
                push("--chip");
                push(chip);
            }
            Tool::Uf2 { .. } => return Vec::new(),
        }
        args.push(elf.as_os_str().to_owned());
        args
    }

    /// Arguments that reset the board and start its program, for a tool
    /// that resets through a probe rather than the serial port.
    pub fn reset_args(&self) -> Option<Vec<OsString>> {
        match self {
            Tool::Espflash { .. } | Tool::Uf2 { .. } => None,
            Tool::ProbeRs { chip } => Some(
                ["reset", "--non-interactive", "--chip", chip]
                    .iter()
                    .map(OsString::from)
                    .collect(),
            ),
        }
    }
}

fn chip_arg(words: &[&str]) -> Option<String> {
    arg(words, "--chip", "-c")
}

/// The value of `--long V`, `--long=V` or `-s V` in a runner.
fn arg(words: &[&str], long: &str, short: &str) -> Option<String> {
    let with_equals = format!("{long}=");
    words.iter().enumerate().find_map(|(i, w)| {
        if let Some(v) = w.strip_prefix(&with_equals) {
            Some(v.to_string())
        } else if *w == long || *w == short {
            words.get(i + 1).map(|v| v.to_string())
        } else {
            None
        }
    })
}

/// `program` on the PATH.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// What a flashing tool printed, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRun {
    pub ok: bool,
    /// stdout then stderr, as text.
    pub output: String,
    pub timed_out: bool,
}

/// Run `program args`, killing it after `limit`.
pub fn run_tool(program: &Path, args: &[OsString], limit: Duration) -> io::Result<ToolRun> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = std::time::Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A killed tool's children can hold its pipes open: after a kill, take
    // what has arrived and stop waiting.
    let wait = if timed_out {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(5)
    };
    let mut output = stdout.map(|r| collect(r, wait)).unwrap_or_default();
    output.push_str(&stderr.map(|r| collect(r, wait)).unwrap_or_default());
    Ok(ToolRun {
        ok: status.is_some_and(|s| s.success()),
        output,
        timed_out,
    })
}

fn drain<R: io::Read + Send + 'static>(mut r: R) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = r.read_to_end(&mut bytes);
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

fn collect(rx: std::sync::mpsc::Receiver<String>, wait: Duration) -> String {
    rx.recv_timeout(wait).unwrap_or_default()
}

/// The two control lines an ESP board's reset circuit listens to.
pub trait ResetLines {
    fn set_dtr(&mut self, level: bool) -> io::Result<()>;
    fn set_rts(&mut self, level: bool) -> io::Result<()>;
}

impl ResetLines for Box<dyn serialport::SerialPort> {
    fn set_dtr(&mut self, level: bool) -> io::Result<()> {
        self.write_data_terminal_ready(level)
            .map_err(io::Error::from)
    }
    fn set_rts(&mut self, level: bool) -> io::Result<()> {
        self.write_request_to_send(level).map_err(io::Error::from)
    }
}

/// Reset an ESP board into its program, the way espflash does after a
/// flash: through the USB-Serial-JTAG peripheral's virtual lines, or the
/// RTS line wired to EN on a USB-UART board. The chip starts when this
/// returns. `pause` is how long each line is held (100 ms on a board).
pub fn reset_esp(lines: &mut impl ResetLines, kind: PortKind, pause: Duration) -> io::Result<()> {
    if kind == PortKind::UsbSerialJtag {
        // The peripheral resets the chip the moment RTS is high with DTR
        // low, and opening the port raised both. RTS comes down first, so
        // the program does not start a pause before time zero.
        lines.set_rts(false)?;
        lines.set_dtr(false)?;
        std::thread::sleep(pause);
        // The reset. RTS twice because Windows applies DTR on RTS. The
        // chip does not wait for RTS to come down, so neither does this.
        lines.set_rts(true)?;
        lines.set_dtr(false)?;
        lines.set_rts(true)?;
        lines.set_rts(false)?;
    } else {
        lines.set_dtr(false)?;
        lines.set_rts(true)?;
        std::thread::sleep(pause);
        lines.set_rts(false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_and_chip_come_from_the_runner() {
        assert_eq!(
            Tool::from_runner("espflash flash --monitor --chip esp32c3").unwrap(),
            Tool::Espflash {
                chip: Some("esp32c3".into())
            }
        );
        assert_eq!(
            Tool::from_runner("espflash flash --monitor").unwrap(),
            Tool::Espflash { chip: None }
        );
        assert_eq!(
            Tool::from_runner("probe-rs run --chip=STM32F401RETx").unwrap(),
            Tool::ProbeRs {
                chip: "STM32F401RETx".into()
            }
        );
        let err = Tool::from_runner("probe-rs run").unwrap_err();
        assert!(err.contains("chip"), "{err}");
        let err = Tool::from_runner("hf2 elf").unwrap_err();
        assert!(err.contains("`hf2 elf`"), "{err}");
    }

    #[test]
    fn a_uf2_runner_names_the_family() {
        assert_eq!(
            Tool::from_runner("elf2uf2-rs -d").unwrap(),
            Tool::Uf2 {
                family: &uf2::RP2040
            }
        );
        assert_eq!(
            Tool::from_runner("uf2deploy deploy -f nrf52840 -p auto").unwrap(),
            Tool::Uf2 {
                family: &uf2::NRF52840
            }
        );
        assert_eq!(
            Tool::from_runner("uf2deploy deploy --family=0xADA52840 -p auto").unwrap(),
            Tool::Uf2 {
                family: &uf2::NRF52840
            }
        );
        let err = Tool::from_runner("uf2deploy deploy -p auto").unwrap_err();
        assert!(err.contains("family"), "{err}");
        let err = Tool::from_runner("uf2deploy deploy -f samd21 -p auto").unwrap_err();
        assert!(
            err.contains("rp2040, nrf52840") && err.contains("samd21"),
            "{err}"
        );
        let t = Tool::from_runner("elf2uf2-rs -d").unwrap();
        assert!(!t.external() && !t.wants(PortKind::Other) && t.reset_args().is_none());
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn espflash_holds_the_board_for_ter_to_reset() {
        let t = Tool::Espflash {
            chip: Some("esp32c3".into()),
        };
        assert_eq!(
            strings(t.flash_args(Path::new("/a/fw.elf"), Some("/dev/ttyACM0"), true)),
            [
                "flash",
                "--non-interactive",
                "--chip",
                "esp32c3",
                "--port",
                "/dev/ttyACM0",
                "--after",
                "no-reset",
                "/a/fw.elf"
            ]
        );
        let t = Tool::Espflash { chip: None };
        let args = strings(t.flash_args(Path::new("fw"), None, false));
        assert_eq!(
            args,
            ["flash", "--non-interactive", "--after", "hard-reset", "fw"]
        );
        assert_eq!(t.reset_args(), None);
    }

    #[test]
    fn probe_rs_downloads_then_resets() {
        let t = Tool::ProbeRs {
            chip: "STM32F401RETx".into(),
        };
        assert_eq!(
            strings(t.flash_args(Path::new("fw"), Some("/dev/ttyACM0"), true)),
            [
                "download",
                "--non-interactive",
                "--chip",
                "STM32F401RETx",
                "fw"
            ]
        );
        assert_eq!(
            strings(t.reset_args().unwrap()),
            ["reset", "--non-interactive", "--chip", "STM32F401RETx"]
        );
    }

    #[derive(Default)]
    struct Lines(Vec<(char, bool)>);

    impl ResetLines for Lines {
        fn set_dtr(&mut self, level: bool) -> io::Result<()> {
            self.0.push(('D', level));
            Ok(())
        }
        fn set_rts(&mut self, level: bool) -> io::Result<()> {
            self.0.push(('R', level));
            Ok(())
        }
    }

    #[test]
    fn the_reset_ends_with_rts_released_and_io0_high() {
        let mut jtag = Lines::default();
        reset_esp(&mut jtag, PortKind::UsbSerialJtag, Duration::ZERO).unwrap();
        assert_eq!(
            jtag.0,
            [
                ('R', false),
                ('D', false),
                ('R', true),
                ('D', false),
                ('R', true),
                ('R', false)
            ]
        );
        let mut uart = Lines::default();
        reset_esp(&mut uart, PortKind::UsbUart, Duration::ZERO).unwrap();
        assert_eq!(uart.0, [('D', false), ('R', true), ('R', false)]);
        assert!(
            uart.0.iter().all(|&(l, v)| l != 'D' || !v),
            "IO0 never low: a normal boot, not download mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_is_run_and_its_output_kept() {
        let ok = run_tool(
            Path::new("/bin/sh"),
            &["-c".into(), "echo out; echo err >&2".into()],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(ok.ok && !ok.timed_out);
        assert_eq!(ok.output, "out\nerr\n");
        let slow = run_tool(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 5".into()],
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(!slow.ok && slow.timed_out);
    }
}
