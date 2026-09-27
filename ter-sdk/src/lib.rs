//! Client for the TER Learn site.
//!
//! Everything `ter` asks of the site goes through [`Client`]: the auth
//! header, the error envelope, the version gate. [`project`] is the
//! learner's side: course folders, `ter.toml` and the scaffold on disk.

pub mod bench;
mod client;
pub mod dev;
mod envelope;
mod error;
pub mod exercise;
pub mod pairing;
pub mod project;
pub mod run;
pub mod version;

pub use client::{Client, Course, Enrollments, Lesson, Ping};
pub use envelope::parse_response;
pub use error::Error;
pub use exercise::{Exercise, ExerciseFile, ExerciseRef};

/// The site `ter` talks to unless the config says otherwise.
pub const DEFAULT_SITE_URL: &str = "https://learn.theembeddedrustacean.com";
