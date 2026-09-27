//! Where a program runs: the venue contract.

use std::path::Path;
use std::time::Duration;

use crate::event::{EventKinds, Stimulus};
use crate::recording::Recording;

/// Why a venue could not produce a recording. Never the learner's fault:
/// a run that ends here is posted as `not_run`, not `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueError {
    pub failure: VenueFailure,
    pub message: String,
    /// What the venue printed, for the transcript.
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueFailure {
    /// Not reachable, died mid-capture, or its account is over quota.
    Unavailable,
    BenchOffline,
    FlashFailed,
}

impl VenueError {
    pub fn unavailable(message: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            failure: VenueFailure::Unavailable,
            message: message.into(),
            output: output.into(),
        }
    }
}

/// What a venue did about a wedged port or a failed flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    NotNeeded,
    Recovered,
    /// Left to the provider (a hosted board).
    Provider,
    Failed,
}

pub trait Venue {
    /// `wokwi`, `local`, ...: the run record's `venue`.
    fn name(&self) -> &'static str;
    /// What it sees and can drive.
    fn provides(&self) -> EventKinds;
    /// Flash or load the program.
    fn prepare(&mut self, elf: &Path) -> Result<(), VenueError>;
    fn reset(&mut self) -> Result<(), VenueError>;
    /// Run from reset for `budget` of wall time at most, applying
    /// `stimuli`, and return what was recorded.
    fn run(&mut self, stimuli: &[Stimulus], budget: Duration) -> Result<Recording, VenueError>;
    fn recover(&mut self) -> Recovery;
}
