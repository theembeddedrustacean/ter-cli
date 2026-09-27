//! The local venue: the learner's own board on USB.
//!
//! ter flashes it with the target's tool (espflash or probe-rs, through
//! `ter-flash`) and captures its serial port itself, so the capture is the
//! same whichever tool flashed. A bare board shows serial and nothing else:
//! pin checks and checks that press a button are not seen here.
//!
//! Time zero is the reset. On an ESP board ter owns it: espflash leaves the
//! chip in its bootloader, ter opens the port, then resets the chip through
//! the port's control lines, so the first byte the program prints is in
//! the capture and times are from the real reset. Where ter cannot reset
//! through the port (a probe, a port it does not know) the tool resets the
//! board and a `Reset` is synthesised when the capture starts.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ter_flash::{Port, PortKind, Tool, run_tool};

use crate::event::{Event, EventKind, EventKinds, Kind, Stimulus, from_reset};
use crate::recording::{Capture, Recording};
use crate::venue::{Recovery, Venue, VenueError, VenueFailure};

/// What the flashing tool printed, in the run folder.
pub const FLASH_LOG: &str = "flash.log";
/// The raw bytes the board sent, in the run folder.
pub const SERIAL_LOG: &str = "serial.log";
pub const BAUD: u32 = 115_200;
/// How long a read waits for bytes before looking at the clock again.
const READ_TIMEOUT: Duration = Duration::from_millis(20);
/// How long after the board sent them bytes can reach ter: USB polls a
/// full-speed device every millisecond. A chunk read at `t` is recorded as
/// sent somewhere in the slack before it.
pub const USB_SLACK_US: u64 = 2_000;
/// Longest a flash may take before it counts as failed.
pub const FLASH_LIMIT: Duration = Duration::from_secs(180);

/// A serial connection to a board.
pub trait Link: Send {
    /// Bytes from the board, or `Ok(0)` when none came within the read
    /// timeout. An error means the port is gone.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Reset the board through the port so its program starts when this
    /// returns.
    fn reset(&mut self) -> io::Result<()>;
}

/// A board's serial port, opened with the `serialport` crate.
pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
    kind: PortKind,
}

impl SerialLink {
    pub fn open(port: &Port) -> io::Result<Self> {
        let opened = serialport::new(&port.path, BAUD)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(io::Error::from)?;
        Ok(Self {
            port: opened,
            kind: port.kind,
        })
    }
}

impl Link for SerialLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            // Readable with nothing to read: the other end hung up.
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the port closed",
            )),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn reset(&mut self) -> io::Result<()> {
        // Whatever the bootloader said while flashing is not the program's.
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        ter_flash::reset_esp(&mut self.port, self.kind, Duration::from_millis(100))
    }
}

type Opener = Box<dyn Fn(&Port) -> io::Result<Box<dyn Link>> + Send>;

/// The board on `port` as a venue for one run, in its run folder.
pub struct Local {
    pub tool: Tool,
    /// The tool's program.
    pub program: PathBuf,
    pub port: Port,
    pub run_dir: PathBuf,
    pub timeout_ms: u64,
    /// Filled in by the caller: what the capture header carries.
    pub capture: Capture,
    /// Opens the port; a board stand-in in tests.
    pub open: Opener,
    link: Option<Box<dyn Link>>,
    /// When the program started (ter's reset) or, with a reset ter did not
    /// see, when the capture did.
    origin: Option<Instant>,
}

impl Local {
    pub fn new(
        tool: Tool,
        program: PathBuf,
        port: Port,
        run_dir: PathBuf,
        timeout_ms: u64,
    ) -> Self {
        Self {
            tool,
            program,
            port,
            run_dir,
            timeout_ms,
            capture: Capture::default(),
            open: Box::new(|p| SerialLink::open(p).map(|l| Box::new(l) as Box<dyn Link>)),
            link: None,
            origin: None,
        }
    }

    /// What a bare board on this port shows: serial, and the reset when
    /// ter drives it.
    pub fn provides_on(tool: &Tool, kind: PortKind) -> EventKinds {
        let mut kinds: EventKinds = [Kind::Serial].into();
        if ter_resets(tool, kind) {
            kinds.insert(Kind::Reset);
        }
        kinds
    }

    fn ter_resets(&self) -> bool {
        ter_resets(&self.tool, self.port.kind)
    }

    fn gone_or(&self, failure: VenueFailure, message: String, output: String) -> VenueError {
        if ter_flash::port::present(&self.port.path) {
            VenueError {
                failure,
                message,
                output,
            }
        } else {
            VenueError::unavailable(
                format!(
                    "The board on {} went away (unplugged?): {message}",
                    self.port.path
                ),
                output,
            )
        }
    }
}

fn ter_resets(tool: &Tool, kind: PortKind) -> bool {
    matches!(tool, Tool::Espflash { .. }) && kind.resets()
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The text of `bytes` after `pending`, keeping a character cut at the
/// end of the chunk for the next one.
fn decode(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let cut = match std::str::from_utf8(pending) {
        Ok(_) => pending.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => pending.len(),
    };
    let rest = pending.split_off(cut);
    let text = String::from_utf8_lossy(pending).into_owned();
    *pending = rest;
    text
}

impl Venue for Local {
    fn name(&self) -> &'static str {
        "local"
    }

    fn provides(&self) -> EventKinds {
        Self::provides_on(&self.tool, self.port.kind)
    }

    fn prepare(&mut self, elf: &Path) -> Result<(), VenueError> {
        let args = self
            .tool
            .flash_args(elf, Some(&self.port.path), self.ter_resets());
        let ran = run_tool(&self.program, &args, FLASH_LIMIT).map_err(|e| {
            VenueError::unavailable(format!("Could not run {}: {e}", self.program.display()), "")
        })?;
        let _ = std::fs::write(self.run_dir.join(FLASH_LOG), &ran.output);
        if ran.ok {
            return Ok(());
        }
        let said = ran
            .output
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        let message = if ran.timed_out {
            format!(
                "{} did not finish flashing within {} s",
                self.tool.program(),
                FLASH_LIMIT.as_secs()
            )
        } else {
            format!("{} could not flash the board: {said}", self.tool.program())
        };
        Err(self.gone_or(VenueFailure::FlashFailed, message, ran.output))
    }

    fn reset(&mut self) -> Result<(), VenueError> {
        let mut link = (self.open)(&self.port).map_err(|e| {
            self.gone_or(
                VenueFailure::Unavailable,
                format!("Could not open {}: {e}", self.port.path),
                String::new(),
            )
        })?;
        if self.ter_resets() {
            link.reset().map_err(|e| {
                self.gone_or(
                    VenueFailure::Unavailable,
                    format!("Could not reset the board through {}: {e}", self.port.path),
                    String::new(),
                )
            })?;
            self.origin = Some(Instant::now());
        } else {
            // The capture starts before the tool resets the board, so
            // nothing the program prints is missed; times run from here.
            self.origin = Some(Instant::now());
            if let Some(args) = self.tool.reset_args() {
                let ran = run_tool(&self.program, &args, Duration::from_secs(60)).map_err(|e| {
                    VenueError::unavailable(
                        format!("Could not run {}: {e}", self.program.display()),
                        "",
                    )
                })?;
                if !ran.ok {
                    return Err(self.gone_or(
                        VenueFailure::Unavailable,
                        format!("{} could not reset the board", self.tool.program()),
                        ran.output,
                    ));
                }
            }
        }
        self.link = Some(link);
        Ok(())
    }

    /// Capture the serial port for the check's `timeout_ms` from reset.
    /// A bare board applies no stimulus: checks that need one are unseen.
    fn run(&mut self, _stimuli: &[Stimulus], budget: Duration) -> Result<Recording, VenueError> {
        let started_at = crate::rfc3339_now();
        let (Some(mut link), Some(origin)) = (self.link.take(), self.origin) else {
            return Err(VenueError::unavailable(
                "The board was not reset before the capture",
                "",
            ));
        };
        let end = Duration::from_millis(self.timeout_ms).min(budget);
        let mut events = Vec::new();
        if self.ter_resets() {
            events.push(Event::reset(0));
        }
        let mut raw = Vec::new();
        let mut pending = Vec::new();
        let mut buf = [0u8; 4096];
        let mut last_us = 0;
        let mut lost = None;
        while origin.elapsed() < end {
            match link.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    let now_us = micros(origin.elapsed());
                    raw.extend_from_slice(&buf[..n]);
                    let text = decode(&mut pending, &buf[..n]);
                    if !text.is_empty() {
                        let from = last_us.max(now_us.saturating_sub(USB_SLACK_US));
                        events.push(Event {
                            t_us: from,
                            kind: EventKind::Serial {
                                text,
                                span_us: now_us - from,
                            },
                        });
                    }
                    last_us = now_us;
                }
                Err(e) => {
                    lost = Some(e);
                    break;
                }
            }
        }
        drop(link);
        let _ = std::fs::write(self.run_dir.join(SERIAL_LOG), &raw);
        let printed = crate::strip_ansi(&String::from_utf8_lossy(&raw));
        if let Some(e) = lost {
            return Err(VenueError::unavailable(
                format!(
                    "The board on {} went away during the capture ({e}); unplugged?",
                    self.port.path
                ),
                printed,
            ));
        }

        let end_us = self.timeout_ms * 1_000;
        let mut events = from_reset(events);
        events.retain(|e| e.t_us <= end_us);
        let mut capture = self.capture.clone();
        capture.venue = "local".into();
        capture.provides = self.provides();
        capture.pins = Vec::new();
        capture.stimuli = Vec::new();
        capture.end_us = end_us;
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
    use std::sync::{Arc, Mutex};

    /// A board that prints `script` (bytes at ms after reset), then either
    /// keeps quiet or, with `unplug_at_ms`, goes away.
    struct Board {
        script: Vec<(u64, &'static [u8])>,
        unplug_at_ms: Option<u64>,
        resets: Arc<Mutex<u32>>,
        start: Option<Instant>,
    }

    impl Link for Board {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let start = self.start.expect("read after reset");
            let now = start.elapsed().as_millis() as u64;
            if self.unplug_at_ms.is_some_and(|t| now >= t) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "Broken pipe"));
            }
            if let Some(&(at, bytes)) = self.script.first()
                && now >= at
            {
                self.script.remove(0);
                buf[..bytes.len()].copy_from_slice(bytes);
                return Ok(bytes.len());
            }
            std::thread::sleep(Duration::from_millis(2));
            Ok(0)
        }

        fn reset(&mut self) -> io::Result<()> {
            *self.resets.lock().unwrap() += 1;
            self.start = Some(Instant::now());
            Ok(())
        }
    }

    fn venue(
        dir: &Path,
        kind: PortKind,
        script: Vec<(u64, &'static [u8])>,
        unplug_at_ms: Option<u64>,
    ) -> (Local, Arc<Mutex<u32>>) {
        let resets = Arc::new(Mutex::new(0));
        let r = resets.clone();
        let mut local = Local::new(
            Tool::Espflash {
                chip: Some("esp32c3".into()),
            },
            PathBuf::from("/bin/true"),
            Port {
                path: "/dev/ttyACM0".into(),
                kind,
                usb: None,
                product: None,
            },
            dir.to_path_buf(),
            400,
        );
        local.open = Box::new(move |_| {
            Ok(Box::new(Board {
                script: script.clone(),
                unplug_at_ms,
                resets: r.clone(),
                // A board the tool reset is already running.
                start: (kind == PortKind::Other).then(Instant::now),
            }) as Box<dyn Link>)
        });
        (local, resets)
    }

    #[test]
    fn serial_is_timed_from_ters_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let (mut v, resets) = venue(
            &dir,
            PortKind::UsbSerialJtag,
            vec![
                (0, b"Hello "),
                (0, b"world!\r\n"),
                (150, b"\x1b[32mtick\x1b[0m\n"),
            ],
            None,
        );
        assert_eq!(v.provides(), [Kind::Serial, Kind::Reset].into());
        v.reset().unwrap();
        let rec = v.run(&[], Duration::from_secs(10)).unwrap();
        assert_eq!(*resets.lock().unwrap(), 1);
        assert_eq!(rec.events[0], Event::reset(0));
        let serial: Vec<(u64, u64, &str)> = rec
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Serial { text, span_us } => Some((e.t_us, *span_us, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(serial.len(), 3);
        assert!(serial[0].0 + serial[0].1 < 50_000, "{serial:?}");
        assert!(
            serial[1].0 >= serial[0].0 + serial[0].1,
            "chunks do not overlap: {serial:?}"
        );
        let (t, span, _) = serial[2];
        assert!(t + span >= 150_000 && t + span < 250_000, "{serial:?}");
        assert!(span <= USB_SLACK_US);
        assert_eq!(rec.serial_text(), "Hello world!\r\ntick\n");
        assert_eq!(rec.capture.venue, "local");
        assert_eq!(rec.capture.end_us, 400_000);
        assert!(rec.capture.pins.is_empty() && rec.capture.stimuli.is_empty());
        assert_eq!(
            std::fs::read(dir.join(SERIAL_LOG)).unwrap(),
            b"Hello world!\r\n\x1b[32mtick\x1b[0m\n"
        );
    }

    #[test]
    fn a_board_the_tool_reset_gets_a_synthesised_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let (mut v, resets) = venue(&dir, PortKind::Other, vec![(0, b"hi\n")], None);
        assert_eq!(v.provides(), [Kind::Serial].into());
        v.reset().unwrap();
        let rec = v.run(&[], Duration::from_secs(10)).unwrap();
        assert_eq!(*resets.lock().unwrap(), 0, "ter did not reset it");
        assert_eq!(rec.events[0], Event::reset(0), "synthesised");
        assert_eq!(rec.serial_text(), "hi\n");
    }

    #[test]
    fn a_board_unplugged_mid_capture_is_unavailable_with_what_it_printed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let (mut v, _) = venue(
            &dir,
            PortKind::UsbSerialJtag,
            vec![(0, b"\x1b[1mbooting\x1b[0m\n")],
            Some(100),
        );
        v.reset().unwrap();
        let err = v.run(&[], Duration::from_secs(10)).unwrap_err();
        assert_eq!(err.failure, VenueFailure::Unavailable);
        assert!(err.message.contains("went away"), "{}", err.message);
        assert_eq!(err.output, "booting\n");
        assert_eq!(
            std::fs::read(dir.join(SERIAL_LOG)).unwrap(),
            b"\x1b[1mbooting\x1b[0m\n"
        );
    }

    #[test]
    fn a_failed_flash_on_a_present_port_is_flash_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let (mut v, _) = venue(&dir, PortKind::UsbSerialJtag, vec![], None);
        v.program = PathBuf::from("/bin/false");
        // The port is a file that exists, so the board is still there.
        let port = dir.join("ttyACM0");
        std::fs::write(&port, "").unwrap();
        v.port.path = port.display().to_string();
        let err = v.prepare(Path::new("fw.elf")).unwrap_err();
        assert_eq!(err.failure, VenueFailure::FlashFailed);
        assert!(dir.join(FLASH_LOG).is_file());

        v.port.path = "/dev/ter-gone".into();
        let err = v.prepare(Path::new("fw.elf")).unwrap_err();
        assert_eq!(err.failure, VenueFailure::Unavailable);
        assert!(err.message.contains("went away"), "{}", err.message);
    }

    #[test]
    fn a_character_cut_between_reads_is_kept_whole() {
        let mut pending = Vec::new();
        let e = "é".as_bytes();
        assert_eq!(decode(&mut pending, &[b'a', e[0]]), "a");
        assert_eq!(decode(&mut pending, &[e[1], b'b']), "éb");
        assert!(pending.is_empty());
    }
}
