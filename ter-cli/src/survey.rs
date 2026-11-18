//! What this machine has that a run can use: the boards and probes on USB,
//! UF2 drives, the flashing tools and Wokwi. Looked at once, without
//! opening a port or starting a tool; `ter venues` prints it and `ter
//! doctor` judges it.

use std::path::PathBuf;

use ter_flash::drive::Drive;
use ter_flash::{Port, PortKind};

use crate::hw::{PORT_ENV, UF2_DRIVE_ENV};
use crate::sim::{self, TOKEN_ENV};
use crate::token_store::TokenStore;

/// The flashing tools a board on USB is reached with.
pub const FLASH_TOOLS: [&str; 2] = ["espflash", "probe-rs"];

#[derive(Debug, Clone, Default)]
pub struct Survey {
    /// Each of [`FLASH_TOOLS`] and where it is on the PATH.
    pub tools: Vec<(&'static str, Option<PathBuf>)>,
    /// The serial ports with a board or a probe behind them.
    pub ports: Vec<Port>,
    /// `TER_PORT`, when it names a port.
    pub port_env: Option<String>,
    /// UF2 boards in their bootloader: the mounted drives, and the one
    /// `TER_UF2_DRIVE` names.
    pub drives: Vec<Drive>,
    /// UF2 bootloaders whose drive is not mounted.
    pub unmounted: Vec<PathBuf>,
    pub wokwi_cli: Option<PathBuf>,
    pub wokwi_token: Option<WokwiToken>,
}

/// The learner's Wokwi token: where it is and whether it looks like one.
/// Never the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WokwiToken {
    /// `WOKWI_CLI_TOKEN`, `keychain` or `file`.
    pub source: &'static str,
    pub well_formed: bool,
}

/// A Wokwi CI token is `wok_` and 40 letters and digits.
pub fn looks_like_a_wokwi_token(token: &str) -> bool {
    token
        .strip_prefix("wok_")
        .is_some_and(|rest| rest.len() == 40 && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn wokwi_token() -> Option<WokwiToken> {
    let (token, source) = match std::env::var(TOKEN_ENV) {
        Ok(t) if !t.trim().is_empty() => (t.trim().to_string(), TOKEN_ENV),
        _ => {
            let (token, location) = TokenStore::open_wokwi().ok()?.load().ok()??;
            (token, location.source())
        }
    };
    Some(WokwiToken {
        source,
        well_formed: looks_like_a_wokwi_token(&token),
    })
}

pub fn survey() -> Survey {
    let named = |var: &str| {
        std::env::var(var)
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
    };
    let named_drive =
        named(UF2_DRIVE_ENV).and_then(|p| Drive::at(std::path::Path::new(p.as_str())));
    let mut drives = ter_flash::drive::mounted();
    drives.extend(named_drive.filter(|d| !drives.iter().any(|m| m.path == d.path)));
    Survey {
        tools: FLASH_TOOLS
            .into_iter()
            .map(|t| (t, ter_flash::find_program(t)))
            .collect(),
        ports: ter_flash::port::list()
            .into_iter()
            .filter(|p| p.kind != PortKind::Other)
            .collect(),
        port_env: named(PORT_ENV),
        drives,
        unmounted: ter_flash::drive::unmounted(),
        wokwi_cli: sim::find_cli(),
        wokwi_token: wokwi_token(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wokwi_token_is_known_by_its_shape() {
        let good = format!("wok_{}", "a1B2".repeat(10));
        assert!(looks_like_a_wokwi_token(&good));
        assert!(!looks_like_a_wokwi_token("wok_short"));
        assert!(!looks_like_a_wokwi_token(&good.replace("wok_", "tok_")));
        assert!(!looks_like_a_wokwi_token(&format!("{good}x")));
        assert!(!looks_like_a_wokwi_token(&good.replace('a', "-")));
    }
}
