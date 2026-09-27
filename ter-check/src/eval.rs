//! Judging a recording against a `check.yaml`.
//!
//! Every time in `check.yaml` is milliseconds after reset; every event is
//! microseconds after reset. Windows include both ends. A toggle, or an
//! edge, is one change of level: a 1 Hz blink toggles twice a second. A
//! pulse is a high pulse whose rise and fall both lie inside the window.
//! Serial bytes a venue could only place within a span of time are judged
//! strictly: they count as `within` a limit only if the whole span is, and
//! as `after` a time only if the whole span is.

use ter_telemetry::{EventKind, Level, Recording};

use crate::spec::{Bounds, Check, CheckFile, What, When, Window};
use crate::{CheckStatus, CheckVerdict};

/// One verdict per check, in `check.yaml` order.
pub fn evaluate(file: &CheckFile, rec: &Recording) -> Vec<CheckVerdict> {
    let stimuli_differ = !file.setup.is_empty() && rec.capture.stimuli != file.stimuli();
    file.checks
        .iter()
        .map(|check| {
            let expected = expected(check);
            if let Some(why) = unseen(file, check, rec, stimuli_differ) {
                return CheckVerdict {
                    id: check.id.clone(),
                    status: CheckStatus::NotObservable,
                    expected,
                    observed: why,
                };
            }
            let (pass, observed) = judge(check, rec);
            CheckVerdict {
                id: check.id.clone(),
                status: if pass {
                    CheckStatus::Pass
                } else {
                    CheckStatus::Fail
                },
                expected,
                observed,
            }
        })
        .collect()
}

/// Why `check` could not be seen in `rec`, if it could not.
pub fn unseen(
    file: &CheckFile,
    check: &Check,
    rec: &Recording,
    stimuli_differ: bool,
) -> Option<String> {
    let missing: Vec<String> = file
        .needs(check)
        .into_iter()
        .filter(|k| !rec.capture.provides.contains(k))
        .map(|k| k.to_string())
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "not seen on {}: it cannot see {}",
            rec.capture.venue,
            missing.join(" or ")
        ));
    }
    if let Some(pin) = check.pin()
        && !rec.capture.pins.iter().any(|p| p == pin)
    {
        return Some(format!(
            "not seen on {}: {pin} was not recorded",
            rec.capture.venue
        ));
    }
    if stimuli_differ {
        return Some("not seen: the run's stimuli are not this check.yaml's setup".into());
    }
    None
}

/// The expected half of the sentence: what the check asks for.
pub fn expected(check: &Check) -> String {
    match &check.what {
        What::SerialContains { text, when } => {
            format!("{text:?} on serial {}", when_text(*when))
        }
        What::Is { level, when, .. } => format!("{level} {}", when_text(*when)),
        What::TogglesPerS { bounds, window, .. } => format!(
            "{} toggles per second {}",
            bounds_text(bounds),
            window_text(*window)
        ),
        What::EdgeCount { bounds, window, .. } => format!(
            "{} {} {}",
            bounds_text(bounds),
            if single(bounds) {
                "level change"
            } else {
                "level changes"
            },
            window_text(*window)
        ),
        What::PulseWidthMs { bounds, window, .. } => format!(
            "every high pulse {} ms wide {}",
            bounds_text(bounds),
            window_text(*window)
        ),
    }
}

fn when_text(when: When) -> String {
    match when {
        When::Within(w) => format!("within {w} ms"),
        When::After(a) => format!("from {a} ms on"),
        When::Between(a, b) => format!("between {a} and {b} ms"),
    }
}

fn window_text(Window(a, b): Window) -> String {
    format!("between {a} and {b} ms")
}

fn bounds_text(b: &Bounds) -> String {
    match (b.min, b.max) {
        (Some(lo), Some(hi)) if lo == hi => format!("exactly {}", num(lo)),
        (Some(lo), Some(hi)) => format!("{} to {}", num(lo), num(hi)),
        (Some(lo), None) => format!("at least {}", num(lo)),
        (None, Some(hi)) => format!("at most {}", num(hi)),
        (None, None) => "any number of".into(),
    }
}

fn single(b: &Bounds) -> bool {
    matches!((b.min, b.max), (Some(1.0), Some(1.0)) | (None, Some(1.0)))
}

/// A number as a person writes it: `4`, `3.6`, `0.85`.
pub fn num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

fn ms(t_us: u64) -> String {
    num(t_us as f64 / 1000.0)
}

const MS: u64 = 1_000;

fn judge(check: &Check, rec: &Recording) -> (bool, String) {
    let end_us = rec.capture.end_us;
    match &check.what {
        What::SerialContains { text, when } => serial(rec, text, *when),
        What::Is { pin, level, when } => level_check(&Pin::of(rec, pin), *level, *when, end_us),
        What::TogglesPerS {
            pin,
            bounds,
            window,
        } => {
            let edges = Pin::of(rec, pin).edges_in(*window);
            let seconds = (window.1 - window.0) as f64 / 1000.0;
            let rate = edges.len() as f64 / seconds;
            (
                bounds.contains(rate),
                format!(
                    "{} toggles per second ({} level {})",
                    num(rate),
                    edges.len(),
                    if edges.len() == 1 {
                        "change"
                    } else {
                        "changes"
                    }
                ),
            )
        }
        What::EdgeCount {
            pin,
            bounds,
            window,
        } => {
            let edges = Pin::of(rec, pin).edges_in(*window);
            let observed = match edges.first() {
                None => "no level changes".to_string(),
                Some((t, _)) if edges.len() == 1 => format!("1 level change, at {} ms", ms(*t)),
                Some((t, _)) => {
                    format!("{} level changes, the first at {} ms", edges.len(), ms(*t))
                }
            };
            (bounds.contains(edges.len() as f64), observed)
        }
        What::PulseWidthMs {
            pin,
            bounds,
            window,
        } => {
            let pulses = Pin::of(rec, pin).high_pulses_in(*window);
            if pulses.is_empty() {
                return (false, "no complete high pulse in the window".into());
            }
            if let Some((t, w)) = pulses
                .iter()
                .find(|(_, w)| !bounds.contains(*w as f64 / 1000.0))
            {
                return (false, format!("a {} ms pulse at {} ms", ms(*w), ms(*t)));
            }
            let lo = pulses.iter().map(|p| p.1).min().expect("not empty");
            let hi = pulses.iter().map(|p| p.1).max().expect("not empty");
            let widths = if lo == hi {
                format!("{} ms", ms(lo))
            } else {
                format!("{} to {} ms", ms(lo), ms(hi))
            };
            let n = pulses.len();
            (
                true,
                format!("{n} {}, {widths}", if n == 1 { "pulse" } else { "pulses" }),
            )
        }
    }
}

/// One pin's recorded levels, in time order.
struct Pin {
    levels: Vec<(u64, Level)>,
}

impl Pin {
    fn of(rec: &Recording, name: &str) -> Self {
        let mut levels: Vec<(u64, Level)> = Vec::new();
        for e in &rec.events {
            if let EventKind::PinEdge { name: n, level } = &e.kind
                && n == name
                && levels.last().is_none_or(|(_, l)| l != level)
            {
                levels.push((e.t_us, *level));
            }
        }
        Pin { levels }
    }

    /// The level at `t`, if one was recorded by then.
    fn at(&self, t: u64) -> Option<Level> {
        self.levels
            .iter()
            .take_while(|(at, _)| *at <= t)
            .last()
            .map(|(_, l)| *l)
    }

    /// Changes of level (not the first recorded level) in `[a, b]` µs.
    fn changes(&self, a: u64, b: u64) -> Vec<(u64, Level)> {
        self.levels
            .iter()
            .skip(1)
            .filter(|(t, _)| *t >= a && *t <= b)
            .copied()
            .collect()
    }

    fn edges_in(&self, Window(a, b): Window) -> Vec<(u64, Level)> {
        self.changes(a * MS, b * MS)
    }

    /// `(rise, width)` in µs of each high pulse inside the window.
    fn high_pulses_in(&self, Window(a, b): Window) -> Vec<(u64, u64)> {
        let changes = self.changes(a * MS, b * MS);
        changes
            .windows(2)
            .filter_map(|w| match (w[0], w[1]) {
                ((rise, Level::High), (fall, Level::Low)) => Some((rise, fall - rise)),
                _ => None,
            })
            .collect()
    }
}

fn level_check(pin: &Pin, want: Level, when: When, end_us: u64) -> (bool, String) {
    let (a, b) = match when {
        When::Within(w) => {
            // Reached at some moment by `w`.
            let reached = pin
                .levels
                .iter()
                .find(|(t, l)| *l == want && *t <= w * MS)
                .map(|(t, _)| *t);
            return match (reached, pin.at(w * MS)) {
                (Some(t), _) => (true, format!("{want} at {} ms", ms(t))),
                (None, None) => (false, "no level recorded".into()),
                (None, Some(l)) => (false, format!("{l} for the whole window")),
            };
        }
        When::After(a) => (a * MS, end_us.max(a * MS)),
        When::Between(a, b) => (a * MS, b * MS),
    };
    let Some(start) = pin.at(a) else {
        return (false, "no level recorded".into());
    };
    let changes = pin.changes(a + 1, b);
    if start != want {
        return if changes.is_empty() {
            (false, format!("{start} for the whole window"))
        } else {
            (false, format!("{start} at {} ms", ms(a)))
        };
    }
    match changes.iter().find(|(_, l)| *l != want) {
        Some((t, l)) => (false, format!("{l} at {} ms", ms(*t))),
        None => (true, format!("{want} for the whole window")),
    }
}

/// Serial bytes, each with the span it was sent in.
struct Serial {
    bytes: Vec<u8>,
    /// Per byte: earliest and latest time it could have been sent.
    times: Vec<(u64, u64)>,
}

impl Serial {
    fn of(rec: &Recording) -> Self {
        let mut s = Serial {
            bytes: Vec::new(),
            times: Vec::new(),
        };
        for e in &rec.events {
            if let EventKind::Serial { text, span_us } = &e.kind {
                // Judged as a terminal shows it: colour codes are not text.
                let text = ter_telemetry::strip_ansi(text);
                s.bytes.extend_from_slice(text.as_bytes());
                s.times
                    .extend(std::iter::repeat_n((e.t_us, e.t_us + span_us), text.len()));
            }
        }
        s
    }

    /// Each occurrence of `needle`: when its first byte could have started
    /// and when its last byte had certainly been sent.
    fn occurrences(&self, needle: &str) -> Vec<(u64, u64)> {
        let n = needle.as_bytes();
        if n.is_empty() || n.len() > self.bytes.len() {
            return Vec::new();
        }
        self.bytes
            .windows(n.len())
            .enumerate()
            .filter(|(_, w)| *w == n)
            .map(|(i, _)| (self.times[i].0, self.times[i + n.len() - 1].1))
            .collect()
    }
}

fn at_text(first: u64, last: u64) -> String {
    if first == last {
        format!("at {} ms", ms(first))
    } else {
        format!("between {} and {} ms", ms(first), ms(last))
    }
}

fn serial(rec: &Recording, text: &str, when: When) -> (bool, String) {
    let serial = Serial::of(rec);
    let found = serial.occurrences(text);
    let nothing = || {
        if serial.bytes.is_empty() {
            "nothing on serial".to_string()
        } else {
            "not printed".to_string()
        }
    };
    let pass = |(first, last): (u64, u64)| match when {
        When::Within(w) => last <= w * MS,
        When::After(a) => first >= a * MS,
        When::Between(a, b) => first >= a * MS && last <= b * MS,
    };
    if let Some(&hit) = found.iter().find(|o| pass(**o)) {
        return (true, format!("printed {}", at_text(hit.0, hit.1)));
    }
    let observed = match (when, found.first(), found.last()) {
        (_, None, _) => nothing(),
        (When::After(_), _, Some(last)) => format!("last printed {}", at_text(last.0, last.1)),
        (_, Some(first), _) => format!("first printed {}", at_text(first.0, first.1)),
    };
    (false, observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ter_telemetry::{Capture, Event, Kind, Stimulus, StimulusKind};

    fn rec(events: Vec<Event>, end_ms: u64) -> Recording {
        Recording {
            capture: Capture {
                venue: "wokwi".into(),
                target: "xiao-esp32c3-nostd".into(),
                provides: [Kind::Serial, Kind::Pins, Kind::Reset].into(),
                pins: vec!["user_led".into(), "buzzer".into()],
                end_us: end_ms * MS,
                ..Capture::default()
            },
            events,
        }
    }

    fn check(yaml_assert: &str) -> CheckFile {
        CheckFile::parse(&format!("timeout_ms: 10000\nassert:\n{yaml_assert}")).unwrap()
    }

    fn led(t_ms: f64, level: Level) -> Event {
        Event::pin((t_ms * 1000.0) as u64, "user_led", level)
    }

    /// The LED starts low and toggles at each of `times` (ms).
    fn toggling(times: &[f64]) -> Vec<Event> {
        let mut events = vec![Event::reset(0), led(0.0, Level::Low)];
        let mut level = Level::Low;
        for &t in times {
            level = if level == Level::Low {
                Level::High
            } else {
                Level::Low
            };
            events.push(led(t, level));
        }
        events
    }

    fn one(file: &CheckFile, rec: &Recording) -> CheckVerdict {
        let mut v = evaluate(file, rec);
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    fn verdict(
        status: CheckStatus,
        expected: &str,
        observed: &str,
    ) -> (CheckStatus, String, String) {
        (status, expected.into(), observed.into())
    }

    fn got(v: CheckVerdict) -> (CheckStatus, String, String) {
        (v.status, v.expected, v.observed)
    }

    use CheckStatus::{Fail, NotObservable, Pass};

    #[test]
    fn is_between_holds_for_the_whole_window_with_its_ends_included() {
        let f =
            check("  - id: on\n    pin: user_led\n    is: high\n    between_ms: [1050, 1450]\n");
        // High from exactly 1050 to exactly 1450: passes, both ends included.
        let r = rec(toggling(&[1050.0, 1450.001]), 3000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Pass,
                "high between 1050 and 1450 ms",
                "high for the whole window"
            )
        );
        // Falls at 1450 exactly: the end is inside the window, so it fails.
        let r = rec(toggling(&[1050.0, 1450.0]), 3000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(Fail, "high between 1050 and 1450 ms", "low at 1450 ms")
        );
        // Rises a microsecond late.
        let r = rec(toggling(&[1050.001]), 3000);
        assert_eq!(one(&f, &r).observed, "low at 1050 ms");
        // Never rises: the its-flow sentence.
        let r = rec(toggling(&[]), 3000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Fail,
                "high between 1050 and 1450 ms",
                "low for the whole window"
            )
        );
    }

    #[test]
    fn is_after_holds_to_the_end_of_the_capture() {
        let f = check("  - id: off\n    pin: user_led\n    is: low\n    after_ms: 1600\n");
        let r = rec(toggling(&[1000.0, 1600.0]), 4000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(Pass, "low from 1600 ms on", "low for the whole window")
        );
        let r = rec(toggling(&[1000.0, 1600.0, 3999.0]), 4000);
        assert_eq!(one(&f, &r).observed, "high at 3999 ms");
        let r = rec(toggling(&[1000.0, 1600.0, 4000.5]), 4000);
        assert_eq!(
            one(&f, &r).status,
            Pass,
            "after the capture ended counts for nothing"
        );
    }

    #[test]
    fn is_within_needs_the_level_by_the_limit() {
        let f = check("  - id: up\n    pin: user_led\n    is: high\n    within_ms: 500\n");
        let r = rec(toggling(&[500.0]), 1000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(Pass, "high within 500 ms", "high at 500 ms")
        );
        let r = rec(toggling(&[500.001]), 1000);
        assert_eq!(one(&f, &r).observed, "low for the whole window");
        let r = rec(vec![Event::reset(0)], 1000);
        assert_eq!(one(&f, &r).observed, "no level recorded");
    }

    #[test]
    fn toggles_per_s_counts_level_changes_over_the_window() {
        let f = check(
            "  - id: blink\n    pin: user_led\n    toggles_per_s: { min: 1.8, max: 2.2 }\n    window_ms: [600, 6600]\n",
        );
        // 1 Hz blink, toggling every 500 ms from 500: 12 changes in the
        // window [600, 6600] (1000 ... 6500).
        let times: Vec<f64> = (1..=14).map(|i| i as f64 * 500.0).collect();
        let r = rec(toggling(&times), 7000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Pass,
                "1.8 to 2.2 toggles per second between 600 and 6600 ms",
                "2 toggles per second (12 level changes)"
            )
        );
        // The lesson's slower blink: every 1000 ms.
        let times: Vec<f64> = (1..=7).map(|i| i as f64 * 1000.0).collect();
        let r = rec(toggling(&times), 7000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Fail,
                "1.8 to 2.2 toggles per second between 600 and 6600 ms",
                "1 toggles per second (6 level changes)"
            )
        );
    }

    #[test]
    fn toggle_windows_include_edges_on_their_boundaries() {
        let f = check(
            "  - id: w\n    pin: user_led\n    toggles_per_s: { min: 2 }\n    window_ms: [1000, 2000]\n",
        );
        let r = rec(toggling(&[999.999, 1000.0, 2000.0, 2000.001]), 3000);
        assert_eq!(
            one(&f, &r).observed,
            "2 toggles per second (2 level changes)"
        );
    }

    #[test]
    fn edge_count_sentences() {
        let f = check(
            "  - id: once\n    pin: user_led\n    edge_count: { min: 1, max: 1 }\n    window_ms: [950, 1150]\n",
        );
        let r = rec(toggling(&[1000.0]), 3000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Pass,
                "exactly 1 level change between 950 and 1150 ms",
                "1 level change, at 1000 ms"
            )
        );
        let r = rec(toggling(&[1000.0, 1010.0, 1020.0]), 3000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Fail,
                "exactly 1 level change between 950 and 1150 ms",
                "3 level changes, the first at 1000 ms"
            )
        );
        let r = rec(toggling(&[]), 3000);
        assert_eq!(one(&f, &r).observed, "no level changes");

        let at_most = check(
            "  - id: steady\n    pin: user_led\n    edge_count: { max: 2 }\n    window_ms: [600, 5000]\n",
        );
        assert_eq!(
            one(&at_most, &rec(toggling(&[]), 5000)).expected,
            "at most 2 level changes between 600 and 5000 ms"
        );
    }

    #[test]
    fn the_first_recorded_level_is_not_an_edge() {
        let f = check(
            "  - id: none\n    pin: user_led\n    edge_count: { max: 0 }\n    window_ms: [0, 1000]\n",
        );
        let r = rec(
            vec![
                Event::reset(0),
                led(0.0, Level::High),
                led(0.0, Level::High),
            ],
            1000,
        );
        assert_eq!(one(&f, &r).status, Pass);
    }

    #[test]
    fn pulse_width_bounds_whole_high_pulses_inside_the_window() {
        let f = check(
            "  - id: width\n    pin: user_led\n    pulse_width_ms: { min: 80, max: 120 }\n    window_ms: [900, 2000]\n",
        );
        // A long pulse cut by the window start is ignored; two 100 ms flashes.
        let r = rec(
            toggling(&[500.0, 950.0, 1000.0, 1100.0, 1200.0, 1300.0]),
            2000,
        );
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Pass,
                "every high pulse 80 to 120 ms wide between 900 and 2000 ms",
                "2 pulses, 100 ms"
            )
        );
        let r = rec(toggling(&[1000.0, 1100.0, 1200.0, 1450.0]), 2000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Fail,
                "every high pulse 80 to 120 ms wide between 900 and 2000 ms",
                "a 250 ms pulse at 1200 ms"
            )
        );
        // A pulse still high at the window end is not complete.
        let r = rec(toggling(&[1000.0]), 2000);
        assert_eq!(one(&f, &r).observed, "no complete high pulse in the window");
    }

    #[test]
    fn sub_millisecond_pulses_keep_their_width() {
        let f = check(
            "  - id: servo\n    pin: user_led\n    pulse_width_ms: { max: 1.5 }\n    window_ms: [600, 800]\n",
        );
        let r = rec(toggling(&[620.0, 621.2, 640.0, 641.6]), 1000);
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                Fail,
                "every high pulse at most 1.5 ms wide between 600 and 800 ms",
                "a 1.6 ms pulse at 640 ms"
            )
        );
    }

    fn serial_rec(chunks: &[(u64, &str, u64)]) -> Recording {
        let mut events = vec![Event::reset(0)];
        for (t_ms, text, span_ms) in chunks {
            events.push(Event {
                t_us: t_ms * MS,
                kind: EventKind::Serial {
                    text: text.to_string(),
                    span_us: span_ms * MS,
                },
            });
        }
        rec(events, 5000)
    }

    #[test]
    fn serial_contains_within_after_and_between() {
        let r = serial_rec(&[
            (400, "boot\nReadi", 0),
            (450, "ng: 5\n", 50),
            (3100, "Reading: 6\n", 50),
        ]);
        let within = check("  - id: a\n    serial_contains: \"Reading: \"\n    within_ms: 500\n");
        assert_eq!(
            got(one(&within, &r)),
            verdict(
                Pass,
                "\"Reading: \" on serial within 500 ms",
                "printed between 400 and 500 ms"
            )
        );
        let tight = check("  - id: a\n    serial_contains: \"Reading: \"\n    within_ms: 480\n");
        assert_eq!(
            got(one(&tight, &r)),
            verdict(
                Fail,
                "\"Reading: \" on serial within 480 ms",
                "first printed between 400 and 500 ms"
            ),
            "a span that ends past the limit is not within it"
        );
        let after = check("  - id: a\n    serial_contains: \"Reading: \"\n    after_ms: 3000\n");
        assert_eq!(
            got(one(&after, &r)),
            verdict(
                Pass,
                "\"Reading: \" on serial from 3000 ms on",
                "printed between 3100 and 3150 ms"
            )
        );
        let late = check("  - id: a\n    serial_contains: \"Reading: \"\n    after_ms: 3120\n");
        assert_eq!(
            one(&late, &r).observed,
            "last printed between 3100 and 3150 ms",
            "a span that starts before the time is not after it"
        );
        let between =
            check("  - id: a\n    serial_contains: \"ng: 6\"\n    between_ms: [3000, 3200]\n");
        assert_eq!(one(&between, &r).status, Pass);
        let coloured = serial_rec(&[(400, "\u{1b}[32mINFO - Hello world!\u{1b}[0m\n", 50)]);
        let line =
            check("  - id: a\n    serial_contains: \"Hello world!\\n\"\n    within_ms: 500\n");
        assert_eq!(
            one(&line, &coloured).status,
            Pass,
            "colour codes are not text"
        );
        let never = check("  - id: a\n    serial_contains: \"panic\"\n    within_ms: 5000\n");
        assert_eq!(one(&never, &r).observed, "not printed");
        assert_eq!(one(&never, &serial_rec(&[])).observed, "nothing on serial");
    }

    #[test]
    fn a_check_the_venue_cannot_see_is_not_observable_and_named() {
        let f = check("  - id: on\n    pin: user_led\n    is: high\n    after_ms: 0\n");
        let mut r = rec(toggling(&[]), 1000);
        r.capture.venue = "local".into();
        r.capture.provides = [Kind::Serial, Kind::Reset].into();
        r.capture.pins.clear();
        assert_eq!(
            got(one(&f, &r)),
            verdict(
                NotObservable,
                "high from 0 ms on",
                "not seen on local: it cannot see pins"
            )
        );

        let buzzer =
            check("  - id: tone\n    pin: ldr\n    edge_count: {min: 1}\n    window_ms: [0, 1]\n");
        assert_eq!(
            one(&buzzer, &rec(vec![], 1000)).observed,
            "not seen on wokwi: ldr was not recorded"
        );
    }

    #[test]
    fn a_setup_the_venue_cannot_drive_or_did_not_apply_is_not_observable() {
        let f = CheckFile::parse(
            "timeout_ms: 4000\nsetup:\n  - press: user_button\n    at_ms: 1000\n    hold_ms: 500\nassert:\n  - id: on\n    pin: user_led\n    is: high\n    after_ms: 1100\n",
        )
        .unwrap();
        let mut r = rec(toggling(&[1050.0]), 4000);
        assert_eq!(
            one(&f, &r).observed,
            "not seen on wokwi: it cannot see stimulus"
        );
        r.capture.provides.insert(Kind::Stimulus);
        r.capture.stimuli = vec![Stimulus {
            at_ms: 1000,
            kind: StimulusKind::Press {
                name: "user_button".into(),
            },
        }];
        assert_eq!(one(&f, &r).status, NotObservable, "the release is missing");
        r.capture.stimuli = f.stimuli();
        assert_eq!(one(&f, &r).status, Pass);
    }

    #[test]
    fn numbers_read_naturally() {
        assert_eq!(num(4.0), "4");
        assert_eq!(num(3.6), "3.6");
        assert_eq!(num(0.8), "0.8");
        assert_eq!(num(1200.0), "1200");
        assert_eq!(num(1.2346), "1.235");
    }
}
