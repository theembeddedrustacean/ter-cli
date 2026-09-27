//! Venues, instruments and the event stream.
//!
//! A venue is where a program runs (a local board, a simulator, a remote
//! bench) and an instrument observes or drives lines alongside it. Both
//! record into one event stream that `ter-check` evaluates.
//!
//! This crate must never depend on anything that reaches the network; the
//! local check enforces it. A venue that uses a remote service does so
//! through that service's own command line tool.

pub mod event;
pub mod recording;
pub mod vcd;
pub mod venue;
pub mod wokwi;

pub use event::{Event, EventKind, EventKinds, Kind, Level, Stimulus, StimulusKind};
pub use recording::{Capture, Recording};
pub use venue::{Recovery, Venue, VenueError, VenueFailure};

/// `t` as RFC 3339 in UTC, to the second: `2026-09-27T12:00:00Z`.
pub fn rfc3339_utc(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

pub(crate) fn rfc3339_now() -> String {
    rfc3339_utc(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_rfc3339_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_790_510_400 + 3_723);
        assert_eq!(rfc3339_utc(t), "2026-09-27T13:02:03Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339_utc(leap), "2000-02-29T00:00:00Z");
    }
}
