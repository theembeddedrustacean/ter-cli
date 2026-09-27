//! When `ter serve --share` tells the site its bench is alive, and when
//! the site will be showing it as offline.
//!
//! The site shows a bench offline once it has heard nothing for its TTL
//! (60 s), and asks for a heartbeat every half of that. A failed heartbeat
//! is retried sooner, so one dropped request does not take the bench
//! offline. Pure: the caller sleeps, sends and reports back.

use std::time::{Duration, Instant};

/// Retry a failed heartbeat after this, or sooner if the interval is
/// shorter.
pub const RETRY: Duration = Duration::from_secs(5);

/// The site's view of the bench changed, as far as this side can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// No heartbeat reached the site within its TTL: it shows the bench
    /// as offline now.
    WentOffline,
    /// A heartbeat got through after the bench had gone offline.
    BackOnline,
}

#[derive(Debug, Clone)]
pub struct Schedule {
    every: Duration,
    ttl: Duration,
    /// The last heartbeat the site took; registering counts as one.
    last_ok: Instant,
    offline: bool,
}

impl Schedule {
    /// Just registered at `now`: the site has seen the bench.
    pub fn new(every: Duration, ttl: Duration, now: Instant) -> Self {
        Self {
            every: every.max(Duration::from_secs(1)),
            ttl,
            last_ok: now,
            offline: false,
        }
    }

    /// How long to wait before the first heartbeat.
    pub fn first(&self) -> Duration {
        self.every
    }

    /// The heartbeat sent at `now` got through. Returns the wait before the
    /// next one, and the change if the bench was offline.
    pub fn ok(&mut self, now: Instant) -> (Duration, Option<Change>) {
        self.last_ok = now;
        let change = std::mem::replace(&mut self.offline, false).then_some(Change::BackOnline);
        (self.every, change)
    }

    /// The heartbeat sent at `now` failed. Returns the wait before the
    /// retry, and `WentOffline` the first time the site's TTL has passed
    /// without one.
    pub fn failed(&mut self, now: Instant) -> (Duration, Option<Change>) {
        let change = (!self.offline && self.offline_at(now)).then(|| {
            self.offline = true;
            Change::WentOffline
        });
        (RETRY.min(self.every), change)
    }

    /// Whether the site shows the bench as offline at `now`.
    pub fn offline_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_ok) > self.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn heartbeats_come_every_interval_and_retry_sooner() {
        let t0 = Instant::now();
        let mut hb = Schedule::new(s(30), s(60), t0);
        assert_eq!(hb.first(), s(30));
        assert_eq!(hb.ok(t0 + s(30)), (s(30), None));
        assert_eq!(hb.failed(t0 + s(60)), (RETRY, None));
        assert_eq!(hb.ok(t0 + s(65)), (s(30), None));
        let mut quick = Schedule::new(s(2), s(60), t0);
        assert_eq!(quick.failed(t0 + s(2)).0, s(2));
    }

    #[test]
    fn offline_once_the_sites_ttl_passes_without_a_heartbeat() {
        let t0 = Instant::now();
        let mut hb = Schedule::new(s(30), s(60), t0);
        hb.ok(t0 + s(30));
        assert_eq!(hb.failed(t0 + s(60)).1, None);
        assert_eq!(hb.failed(t0 + s(90)).1, None, "exactly the TTL: still up");
        assert!(!hb.offline_at(t0 + s(90)));
        assert_eq!(hb.failed(t0 + s(91)).1, Some(Change::WentOffline));
        assert_eq!(hb.failed(t0 + s(96)).1, None, "said once");
        assert!(hb.offline_at(t0 + s(96)));
        assert_eq!(hb.ok(t0 + s(101)), (s(30), Some(Change::BackOnline)));
        assert!(!hb.offline_at(t0 + s(101)));
        assert_eq!(hb.ok(t0 + s(131)).1, None);
    }

    #[test]
    fn registering_counts_as_the_first_heartbeat() {
        let t0 = Instant::now();
        let mut hb = Schedule::new(s(30), s(60), t0);
        assert_eq!(hb.failed(t0 + s(30)).1, None);
        assert_eq!(hb.failed(t0 + s(61)).1, Some(Change::WentOffline));
    }
}
