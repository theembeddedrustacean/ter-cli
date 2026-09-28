//! Serial ports with a board behind them.

/// What sits behind a USB serial port, from its vendor and product ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum PortKind {
    /// An ESP chip's own USB (XIAO ESP32-C3, -C6): serial and reset in one.
    UsbSerialJtag,
    /// A USB-UART bridge (CP210x, CH340, FTDI) with RTS on the chip's EN.
    UsbUart,
    /// A debug probe's virtual COM port (ST-LINK on a Nucleo).
    Probe,
    /// Anything else, including ports that are not on USB.
    #[default]
    Other,
}

impl PortKind {
    pub fn of(vid: u16, pid: u16) -> Self {
        match (vid, pid) {
            (0x303a, 0x1001) => PortKind::UsbSerialJtag,
            (0x10c4, 0xea60 | 0xea70)
            | (0x1a86, 0x7523 | 0x55d4 | 0x55d3)
            | (0x0403, 0x6001 | 0x6010 | 0x6014 | 0x6015) => PortKind::UsbUart,
            (0x0483, 0x374b | 0x374e | 0x374f | 0x3752 | 0x3753 | 0x3754) => PortKind::Probe,
            _ => PortKind::Other,
        }
    }

    /// Whether ter can reset the board through this port's control lines.
    pub fn resets(self) -> bool {
        matches!(self, PortKind::UsbSerialJtag | PortKind::UsbUart)
    }
}

impl std::fmt::Display for PortKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PortKind::UsbSerialJtag => "ESP USB-Serial-JTAG",
            PortKind::UsbUart => "USB-UART bridge",
            PortKind::Probe => "debug probe",
            PortKind::Other => "serial port",
        })
    }
}

/// A serial port and what is behind it. The default, with no path, is a
/// port not known yet: a UF2 board's program shows up on one only after the
/// flash.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Port {
    pub path: String,
    pub kind: PortKind,
    /// `vid:pid` in hex, for a USB port.
    pub usb: Option<String>,
    pub product: Option<String>,
}

impl Port {
    /// A port named by the learner (`TER_PORT`), described if it is on USB.
    pub fn named(path: &str) -> Self {
        list()
            .into_iter()
            .find(|p| p.path == path)
            .unwrap_or_else(|| Port {
                path: path.to_string(),
                kind: PortKind::Other,
                usb: None,
                product: None,
            })
    }
}

/// Every serial port on USB, boards first.
pub fn list() -> Vec<Port> {
    let mut ports: Vec<Port> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(usb) => Some(Port {
                path: p.port_name,
                kind: PortKind::of(usb.vid, usb.pid),
                usb: Some(format!("{:04x}:{:04x}", usb.vid, usb.pid)),
                product: usb.product,
            }),
            _ => None,
        })
        .collect();
    ports.sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));
    ports
}

/// Watches for the port a board's program shows up on after its
/// bootloader restarts it: one that was not there before, or one that went
/// away and came back (a bootloader's port and the program's can share a
/// path, and even ids).
pub struct Appearing {
    before: Vec<(String, Option<String>)>,
    gone: Vec<(String, Option<String>)>,
}

impl Appearing {
    /// Start from the ports there now.
    pub fn from(before: &[Port]) -> Self {
        Self {
            before: before.iter().map(key).collect(),
            gone: Vec::new(),
        }
    }

    /// The new port among `now`, if one has appeared.
    pub fn step(&mut self, now: &[Port]) -> Option<Port> {
        let now_keys: Vec<_> = now.iter().map(key).collect();
        for k in &self.before {
            if !now_keys.contains(k) && !self.gone.contains(k) {
                self.gone.push(k.clone());
            }
        }
        now.iter()
            .find(|p| {
                let k = key(p);
                !self.before.contains(&k) || self.gone.contains(&k)
            })
            .cloned()
    }
}

fn key(p: &Port) -> (String, Option<String>) {
    (p.path.clone(), p.usb.clone())
}

/// Whether `path` is still there: a board that was unplugged is not.
pub fn present(path: &str) -> bool {
    std::path::Path::new(path).exists()
        || serialport::available_ports()
            .unwrap_or_default()
            .iter()
            .any(|p| p.port_name == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boards_are_known_by_their_usb_ids() {
        assert_eq!(PortKind::of(0x303a, 0x1001), PortKind::UsbSerialJtag);
        assert_eq!(PortKind::of(0x10c4, 0xea60), PortKind::UsbUart);
        assert_eq!(PortKind::of(0x1a86, 0x7523), PortKind::UsbUart);
        assert_eq!(PortKind::of(0x0483, 0x374b), PortKind::Probe);
        assert_eq!(PortKind::of(0x2e8a, 0x000a), PortKind::Other);
        assert!(PortKind::UsbSerialJtag.resets() && PortKind::UsbUart.resets());
        assert!(!PortKind::Probe.resets() && !PortKind::Other.resets());
    }

    #[test]
    fn the_programs_port_is_the_one_that_appears() {
        let port = |path: &str, usb: &str| Port {
            path: path.into(),
            kind: PortKind::Other,
            usb: Some(usb.into()),
            product: None,
        };
        let esp = port("/dev/ttyACM0", "303a:1001");
        let boot = port("/dev/ttyACM1", "2886:0045");
        let mut w = Appearing::from(&[esp.clone(), boot.clone()]);
        assert_eq!(w.step(&[esp.clone(), boot.clone()]), None);
        let program = port("/dev/ttyACM1", "2886:8045");
        assert_eq!(w.step(&[esp.clone(), program.clone()]), Some(program));

        // Same path, same ids: it has to go away first.
        let alone = std::slice::from_ref(&boot);
        let mut w = Appearing::from(alone);
        assert_eq!(w.step(alone), None);
        assert_eq!(w.step(&[]), None);
        assert_eq!(w.step(alone), Some(boot.clone()));
    }

    #[test]
    fn a_named_port_that_is_not_on_usb_is_other() {
        let p = Port::named("/dev/ter-no-such-port");
        assert_eq!(p.kind, PortKind::Other);
        assert!(!present("/dev/ter-no-such-port"));
    }
}
