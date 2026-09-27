//! Serial ports with a board behind them.

/// What sits behind a USB serial port, from its vendor and product ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortKind {
    /// An ESP chip's own USB (XIAO ESP32-C3, -C6): serial and reset in one.
    UsbSerialJtag,
    /// A USB-UART bridge (CP210x, CH340, FTDI) with RTS on the chip's EN.
    UsbUart,
    /// A debug probe's virtual COM port (ST-LINK on a Nucleo).
    Probe,
    /// Anything else, including ports that are not on USB.
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

/// A serial port and what is behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn a_named_port_that_is_not_on_usb_is_other() {
        let p = Port::named("/dev/ter-no-such-port");
        assert_eq!(p.kind, PortKind::Other);
        assert!(!present("/dev/ter-no-such-port"));
    }
}
