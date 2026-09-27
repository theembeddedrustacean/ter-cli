//! The config, the device token and a site client, built once per command.

use ter_sdk::Client;
use ter_sdk::version::{check_supported, newer_available};

use crate::config::Config;
use crate::output::CliError;

pub struct Session {
    pub client: Client,
    /// Where the token came from, for display. Never the token itself.
    pub token_source: &'static str,
}

impl Session {
    pub fn open() -> Result<Self, CliError> {
        let config = Config::load()?;
        let (token, token_source) = match std::env::var("TER_TOKEN") {
            Ok(t) if !t.trim().is_empty() => (Some(t.trim().to_string()), "TER_TOKEN"),
            _ => (None, "none"),
        };
        let client = Client::new(&config.site_url, token, env!("CARGO_PKG_VERSION"))?;
        Ok(Self {
            client,
            token_source,
        })
    }

    /// "ter X.Y.Z is available" when this command's `ping` said so. Nothing
    /// when the command never pinged, and nothing when ter is below the
    /// site's minimum: the `outdated` message already says to update.
    pub fn newer_version_notice(&self) -> Option<String> {
        let ping = self.client.cached_ping()?;
        let current = self.client.cli_version();
        check_supported(current, &ping.min_supported_version).ok()?;
        let latest = newer_available(current, &ping.latest_version)?;
        Some(format!("ter {latest} is available, run ter self-update."))
    }
}
