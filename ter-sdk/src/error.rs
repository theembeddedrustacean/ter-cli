/// Everything that can go wrong talking to the site.
///
/// [`Error::code`] is the stable string `ter` prints and exits with. Site
/// codes are passed through as the site sent them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The site answered with its error envelope.
    #[error("{message}")]
    Site {
        code: String,
        message: String,
        http_status: u16,
    },

    /// The site failed in a way it did not describe (a Python exception, a
    /// proxy error page). This is a site bug to report, not a learner error.
    #[error("The site had an internal error (HTTP {http_status}{}). Please report this.", exc_type.as_deref().map(|t| format!(", {t}")).unwrap_or_default())]
    Server {
        http_status: u16,
        exc_type: Option<String>,
    },

    /// The site answered, but not in the shape `ter` expects.
    #[error(
        "The site's answer to `{function}` was not what ter expects ({detail}). Please report this."
    )]
    UnexpectedAnswer { function: String, detail: String },

    #[error("Could not reach the site: {0}")]
    Network(String),

    #[error("No device token. Run `ter login`, or set TER_TOKEN.")]
    NoToken,

    #[error(
        "The pairing code expired before it was approved. Run `ter login` again for a fresh one."
    )]
    PairingExpired,

    #[error(
        "ter {current} is older than the oldest version the site accepts ({minimum}). Run `ter self-update`."
    )]
    Outdated { current: String, minimum: String },
}

impl Error {
    pub fn code(&self) -> &str {
        match self {
            Error::Site { code, .. } => code,
            Error::Server { .. } | Error::UnexpectedAnswer { .. } => "server_error",
            Error::Network(_) => "network_error",
            Error::NoToken => "no_token",
            Error::PairingExpired => "pairing_expired",
            Error::Outdated { .. } => "outdated",
        }
    }
}
