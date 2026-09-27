//! Client for the TER Learn site.
//!
//! Everything `ter` asks of the site goes through [`Client`]: the auth
//! header, the error envelope, the version gate.

mod client;
mod envelope;
mod error;
pub mod pairing;
pub mod version;

pub use client::{Client, Course, Enrollments, Lesson, Ping};
pub use envelope::parse_response;
pub use error::Error;

/// The site `ter` talks to unless the config says otherwise.
pub const DEFAULT_SITE_URL: &str = "https://learn.theembeddedrustacean.com";
