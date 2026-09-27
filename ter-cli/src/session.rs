//! The config, the device token and a site client, built once per command.

use ter_sdk::Client;
use ter_sdk::version::{check_supported, newer_available};

use crate::config::Config;
use crate::output::CliError;
use crate::token_store::TokenStore;

pub struct Session {
    pub client: Client,
    /// Where the token came from, for display: `TER_TOKEN`, `keychain`,
    /// `file` or `none`. Never the token itself.
    pub token_source: &'static str,
    pub store: TokenStore,
}

impl Session {
    /// `TER_TOKEN` when set, else the token `ter login` stored.
    pub fn open() -> Result<Self, CliError> {
        let config = Config::load()?;
        let store = TokenStore::open()?;
        let (token, token_source) = match std::env::var("TER_TOKEN") {
            Ok(t) if !t.trim().is_empty() => (Some(t.trim().to_string()), "TER_TOKEN"),
            _ => match store.load()? {
                Some((token, location)) => (Some(token), location.source()),
                None => (None, "none"),
            },
        };
        let client = Client::new(&config.site_url, token, env!("CARGO_PKG_VERSION"))?;
        Ok(Self {
            client,
            token_source,
            store,
        })
    }

    /// When the site says the stored token was revoked or has expired,
    /// remove it from this machine and say so. A token from `TER_TOKEN`
    /// is left to whoever set it.
    pub fn forget_dead_token(&self, mut err: CliError) -> CliError {
        let stored = matches!(self.token_source, "keychain" | "file");
        if !stored || !is_dead_token(&err) {
            return err;
        }
        match self.store.clear() {
            Ok(_) => err
                .message
                .push_str(" Removed it from this machine. Run `ter login` to pair again."),
            Err(e) => err.message.push_str(&format!(
                " Could not remove it from this machine ({}). Run `ter logout`, then `ter login`.",
                e.message
            )),
        }
        err
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

/// The site's 401 for a device revoked on the Devices page, or one past
/// its lifetime. An unknown token is not dead in this sense: it may be a
/// typo in `TER_TOKEN`, and `ter login` replaces a stored one anyway.
fn is_dead_token(err: &CliError) -> bool {
    err.code == "token_invalid"
        && (err.message.starts_with("Token revoked") || err.message.starts_with("Token expired"))
}
