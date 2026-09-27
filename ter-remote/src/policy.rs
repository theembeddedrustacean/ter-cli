//! The owner's rules for a shared bench: one driver at a time, for a
//! limited session, and a kill switch that drops whoever is driving.
//!
//! Pure: every call takes the time, so the rules are tested without a
//! clock. The bench server holds one [`DriverLock`] behind a mutex.

use std::time::{Duration, Instant};

/// How long one driver may hold the bench.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A driver that has not run anything for this long lets the bench go.
    pub idle: Duration,
    /// Nobody holds the bench longer than this in one go; then anyone may
    /// take it, the same driver included.
    pub session: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(120),
            session: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    driver: String,
    since: Instant,
    last: Instant,
    /// A run is in flight.
    running: bool,
}

/// Who drives the bench now.
#[derive(Debug, Clone)]
pub struct DriverLock {
    limits: Limits,
    holder: Option<Holder>,
}

/// The answer to a driver asking for the bench.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The bench is this driver's for one run. `new_session` when they did
    /// not hold it before: the board carries nothing over from the last
    /// driver, since every run flashes and resets it.
    Granted { new_session: bool },
    /// Someone else holds it; free in `free_in` at the latest (sooner if
    /// they stop).
    Busy { driver: String, free_in: Duration },
}

impl DriverLock {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            holder: None,
        }
    }

    /// Let go of a holder whose session is over or who went quiet. A run
    /// in flight is never cut off; the limits apply once it ends.
    fn expire(&mut self, now: Instant) {
        if let Some(h) = &self.holder
            && !h.running
            && (now.saturating_duration_since(h.last) >= self.limits.idle
                || now.saturating_duration_since(h.since) >= self.limits.session)
        {
            self.holder = None;
        }
    }

    /// `driver` wants to run now. Granted marks the run in flight until
    /// [`DriverLock::finished`].
    pub fn claim(&mut self, driver: &str, now: Instant) -> Claim {
        self.expire(now);
        match &mut self.holder {
            Some(h) if h.driver == driver => {
                if h.running {
                    return Claim::Busy {
                        driver: driver.to_string(),
                        free_in: self.limits.idle,
                    };
                }
                h.running = true;
                h.last = now;
                Claim::Granted { new_session: false }
            }
            Some(h) => {
                let idle_end = if h.running {
                    self.limits.idle
                } else {
                    self.limits
                        .idle
                        .saturating_sub(now.saturating_duration_since(h.last))
                };
                let session_end = self
                    .limits
                    .session
                    .saturating_sub(now.saturating_duration_since(h.since));
                Claim::Busy {
                    driver: h.driver.clone(),
                    free_in: idle_end.min(session_end),
                }
            }
            None => {
                self.holder = Some(Holder {
                    driver: driver.to_string(),
                    since: now,
                    last: now,
                    running: true,
                });
                Claim::Granted { new_session: true }
            }
        }
    }

    /// `driver`'s run ended; they keep the bench until idle or the session
    /// cap.
    pub fn finished(&mut self, driver: &str, now: Instant) {
        if let Some(h) = &mut self.holder
            && h.driver == driver
        {
            h.running = false;
            h.last = now;
        }
    }

    /// The driver now, if anyone holds the bench.
    pub fn driver(&mut self, now: Instant) -> Option<&str> {
        self.expire(now);
        self.holder.as_ref().map(|h| h.driver.as_str())
    }

    /// Whether a run is in flight.
    pub fn running(&self) -> bool {
        self.holder.as_ref().is_some_and(|h| h.running)
    }

    /// The kill switch: whoever holds the bench loses it now, mid-run or
    /// not. Returns who that was.
    pub fn kill(&mut self) -> Option<String> {
        self.holder.take().map(|h| h.driver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        idle: Duration::from_secs(120),
        session: Duration::from_secs(900),
    };

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn one_driver_at_a_time() {
        let t0 = Instant::now();
        let mut lock = DriverLock::new(LIMITS);
        assert_eq!(lock.claim("ana", t0), Claim::Granted { new_session: true });
        assert!(lock.running());
        assert_eq!(
            lock.claim("ben", t0 + s(1)),
            Claim::Busy {
                driver: "ana".into(),
                free_in: s(120)
            },
            "mid-run, Ben waits at least a whole idle period"
        );
        lock.finished("ana", t0 + s(10));
        assert!(!lock.running());
        assert_eq!(
            lock.claim("ben", t0 + s(40)),
            Claim::Busy {
                driver: "ana".into(),
                free_in: s(90)
            }
        );
        assert_eq!(
            lock.claim("ana", t0 + s(50)),
            Claim::Granted { new_session: false }
        );
        assert!(
            matches!(lock.claim("ana", t0 + s(51)), Claim::Busy { .. }),
            "not two runs of one driver at once"
        );
    }

    #[test]
    fn a_quiet_driver_lets_the_bench_go() {
        let t0 = Instant::now();
        let mut lock = DriverLock::new(LIMITS);
        lock.claim("ana", t0);
        lock.finished("ana", t0 + s(5));
        assert_eq!(lock.driver(t0 + s(124)), Some("ana"));
        assert_eq!(lock.driver(t0 + s(125)), None);
        assert_eq!(
            lock.claim("ben", t0 + s(125)),
            Claim::Granted { new_session: true }
        );
    }

    #[test]
    fn the_session_cap_frees_the_bench_even_for_a_busy_driver() {
        let t0 = Instant::now();
        let mut lock = DriverLock::new(LIMITS);
        let mut t = t0;
        lock.claim("ana", t);
        // Ana runs every minute and never goes idle.
        while t < t0 + s(900) {
            lock.finished("ana", t + s(30));
            t += s(60);
            if t >= t0 + s(900) {
                break;
            }
            assert_eq!(lock.claim("ana", t), Claim::Granted { new_session: false });
        }
        assert_eq!(
            lock.claim("ben", t0 + s(900)),
            Claim::Granted { new_session: true },
            "Ben gets it once Ana's 15 minutes are up"
        );
    }

    #[test]
    fn a_run_in_flight_is_never_expired_under_the_driver() {
        let t0 = Instant::now();
        let mut lock = DriverLock::new(LIMITS);
        lock.claim("ana", t0);
        assert_eq!(lock.driver(t0 + s(1000)), Some("ana"));
        lock.finished("ana", t0 + s(1000));
        assert_eq!(lock.driver(t0 + s(1000)), None, "past the session cap");
    }

    #[test]
    fn the_kill_switch_drops_the_driver_mid_run() {
        let t0 = Instant::now();
        let mut lock = DriverLock::new(LIMITS);
        lock.claim("ana", t0);
        assert_eq!(lock.kill().as_deref(), Some("ana"));
        assert!(!lock.running());
        assert_eq!(lock.driver(t0), None);
        // Ana's run ending after the kill does not give her the bench back.
        lock.finished("ana", t0 + s(3));
        assert_eq!(lock.driver(t0 + s(3)), None);
        assert_eq!(
            lock.claim("ben", t0 + s(4)),
            Claim::Granted { new_session: true }
        );
        assert_eq!(DriverLock::new(LIMITS).kill(), None);
    }
}
