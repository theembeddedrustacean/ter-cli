//! Device pairing: `pair_start`, then `pair_poll` until the learner approves
//! the code on the site's `/cli` page.

use std::time::Duration;

use serde::Deserialize;

use crate::Error;

/// How often to ask whether the code has been approved.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// The longest wait between polls while the site cannot be reached.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The site's answer to `pair_start`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PairStart {
    pub code: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
}

/// The site's answer to `pair_poll`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PollStatus {
    Pending,
    /// The token is handed out once; the next poll says `expired`.
    Approved {
        token: String,
    },
    /// Unknown, used or expired code.
    Expired,
}

/// What the caller does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Sleep this long, then poll again.
    Wait(Duration),
    /// Paired; store the token.
    Done(String),
    Fail(Error),
}

/// The polling state machine, without the sleeping or the requests, so it
/// can be driven by tests.
///
/// Pending waits [`POLL_INTERVAL`]. A failure that may pass (the network,
/// a site error, `rate_limited`) backs off, doubling up to [`MAX_BACKOFF`].
/// Once the code's lifetime has passed with no answer, polling stops.
#[derive(Debug)]
pub struct Poller {
    interval: Duration,
    lifetime: Duration,
    waited: Duration,
    failures: u32,
}

impl Poller {
    pub fn new(expires_in: Duration) -> Self {
        Self::with_interval(expires_in, POLL_INTERVAL)
    }

    pub fn with_interval(expires_in: Duration, interval: Duration) -> Self {
        Self {
            interval,
            // One more interval, so the site gets to say `expired` itself.
            lifetime: expires_in + interval,
            waited: Duration::ZERO,
            failures: 0,
        }
    }

    pub fn next(&mut self, poll: Result<PollStatus, Error>) -> Next {
        let wait = match poll {
            Ok(PollStatus::Approved { token }) => return Next::Done(token),
            Ok(PollStatus::Expired) => return Next::Fail(Error::PairingExpired),
            Ok(PollStatus::Pending) => {
                self.failures = 0;
                self.interval
            }
            Err(e) if retryable(&e) => {
                self.failures += 1;
                if self.waited >= self.lifetime {
                    return Next::Fail(e);
                }
                self.interval
                    .saturating_mul(1 << self.failures.min(5))
                    .min(MAX_BACKOFF)
            }
            Err(e) => return Next::Fail(e),
        };
        if self.waited >= self.lifetime {
            return Next::Fail(Error::PairingExpired);
        }
        self.waited += wait;
        Next::Wait(wait)
    }
}

fn retryable(e: &Error) -> bool {
    match e {
        Error::Network(_) | Error::Server { .. } => true,
        Error::Site { code, .. } => code == "rate_limited",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MIN: Duration = Duration::from_secs(600);

    fn network() -> Result<PollStatus, Error> {
        Err(Error::Network("connection refused".into()))
    }

    #[test]
    fn pending_waits_the_interval() {
        let mut p = Poller::new(TEN_MIN);
        for _ in 0..5 {
            assert_eq!(p.next(Ok(PollStatus::Pending)), Next::Wait(POLL_INTERVAL));
        }
    }

    #[test]
    fn approved_hands_over_the_token() {
        let mut p = Poller::new(TEN_MIN);
        p.next(Ok(PollStatus::Pending));
        assert_eq!(
            p.next(Ok(PollStatus::Approved { token: "t".into() })),
            Next::Done("t".into())
        );
    }

    #[test]
    fn expired_stops_with_pairing_expired() {
        let mut p = Poller::new(TEN_MIN);
        let Next::Fail(e) = p.next(Ok(PollStatus::Expired)) else {
            panic!("expected a failure");
        };
        assert_eq!(e.code(), "pairing_expired");
        assert!(e.to_string().contains("ter login"), "{e}");
    }

    #[test]
    fn network_errors_back_off_then_reset_on_an_answer() {
        let mut p = Poller::new(TEN_MIN);
        let waits: Vec<_> = (0..6)
            .map(|_| match p.next(network()) {
                Next::Wait(d) => d.as_secs(),
                other => panic!("expected a wait, got {other:?}"),
            })
            .collect();
        assert_eq!(waits, [6, 12, 24, 30, 30, 30]);
        assert_eq!(p.next(Ok(PollStatus::Pending)), Next::Wait(POLL_INTERVAL));
        assert_eq!(p.next(network()), Next::Wait(Duration::from_secs(6)));
    }

    #[test]
    fn rate_limited_and_server_errors_are_retried() {
        let mut p = Poller::new(TEN_MIN);
        let limited = Error::Site {
            code: "rate_limited".into(),
            message: "Slow down.".into(),
            http_status: 429,
        };
        assert!(matches!(p.next(Err(limited)), Next::Wait(_)));
        let server = Error::Server {
            http_status: 502,
            exc_type: None,
        };
        assert!(matches!(p.next(Err(server)), Next::Wait(_)));
    }

    #[test]
    fn an_answer_ter_does_not_understand_is_not_retried() {
        let mut p = Poller::new(TEN_MIN);
        let odd = Error::UnexpectedAnswer {
            function: "pair_poll".into(),
            detail: "missing field".into(),
        };
        assert_eq!(p.next(Err(odd.clone())), Next::Fail(odd));
    }

    #[test]
    fn gives_up_after_the_code_lifetime() {
        let mut p = Poller::new(Duration::from_secs(9));
        // 9 s lifetime plus one 3 s grace interval: four waits, then stop.
        for _ in 0..4 {
            assert_eq!(p.next(Ok(PollStatus::Pending)), Next::Wait(POLL_INTERVAL));
        }
        assert_eq!(
            p.next(Ok(PollStatus::Pending)),
            Next::Fail(Error::PairingExpired)
        );
    }

    #[test]
    fn an_outage_past_the_lifetime_reports_the_outage() {
        let mut p = Poller::new(Duration::from_secs(3));
        let last = loop {
            match p.next(network()) {
                Next::Wait(_) => continue,
                Next::Fail(e) => break e,
                Next::Done(_) => unreachable!(),
            }
        };
        assert_eq!(last.code(), "network_error");
    }

    #[test]
    fn poll_answers_parse() {
        let parse = |s: &str| serde_json::from_str::<PollStatus>(s).unwrap();
        assert_eq!(parse(r#"{"status":"pending"}"#), PollStatus::Pending);
        assert_eq!(parse(r#"{"status":"expired"}"#), PollStatus::Expired);
        assert_eq!(
            parse(r#"{"status":"approved","token":"abc"}"#),
            PollStatus::Approved {
                token: "abc".into()
            }
        );
        assert!(serde_json::from_str::<PollStatus>(r#"{"status":"approved"}"#).is_err());
    }
}
