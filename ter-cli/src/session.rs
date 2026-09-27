//! The config, the device token and a site client, built once per command.

use ter_sdk::Client;

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
}
