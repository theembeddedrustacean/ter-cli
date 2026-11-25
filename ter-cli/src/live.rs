//! A live run: the exercise's program on its sim86 board, shown as it
//! happens instead of played back afterwards.
//!
//! The build is `ter run`'s. Then sim86 runs the board with no time limit
//! of its own and ter holds the limit, since sim86 charges for every
//! minute a board runs: the run ends when its time is up (a minute unless
//! it is extended, ten at most), when it is stopped, when a newer run
//! replaces it, or when nobody has listened to it for a while.
//!
//! The checks are not judged from the stream. Once the check's window has
//! run, its recording is read back in full, written to the run folder,
//! judged and posted, exactly as `ter run --sim sim86` does; the board
//! then carries on. A run that ends before the window is over, whatever
//! ended it, is not an attempt: nothing is judged and nothing is posted.
//!
//! ter's helper does the talking to sim86 (`live` mode): it prints a line
//! per pin change and serial line, and stops the run when ter says so or
//! goes away.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use ter_remote::serve::{LIVE_CAP_S, LiveEvent, LiveEvents};
use ter_telemetry::Venue;
use ter_telemetry::event::Stimulus;
use ter_telemetry::sim86::{
    self, Answer, ERROR_LOG, FIRMWARE, Failure, FailureKind, LiveLine, OUTPUT_LOG, ProgramLines,
    REQUEST_FILE, Sim86,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, watch};

use crate::output::CliError;
use crate::run::{self, Built, Done, LiveParts, RunArgs};

/// How long the helper may take to get the run going: the upload, and
/// the wait for a free simulator.
const START_BUDGET: Duration = Duration::from_secs(420);
/// How long the helper may take to stop a run it was told to stop.
const STOP_PATIENCE: Duration = Duration::from_secs(30);
/// How long a run goes on with nobody listening to its events.
const UNWATCHED: Duration = Duration::from_secs(15);
/// How long the site may take to take a run's checks.
const POST_PATIENCE: Duration = Duration::from_secs(60);
/// sim86's own wall-clock limit, past ter's cap: what ends a run whose
/// ter and helper are both gone.
const LAST_RESORT_MS: u64 = (LIVE_CAP_S + 60) * 1_000;

/// [`UNWATCHED`], or `TER_LIVE_UNWATCHED_SECS` when set (tests set it
/// short).
fn unwatched() -> Duration {
    std::env::var("TER_LIVE_UNWATCHED_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(UNWATCHED, Duration::from_secs)
}

/// A wait, or the environment's `name` in seconds when set (tests set
/// them short).
fn patience(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(default, Duration::from_secs)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A name for a run that cannot be guessed: whoever has it may see the
/// run and its sim86 page. From the randomness the standard library seeds
/// its hash maps with.
fn new_id() -> String {
    let word = || {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    format!("{:016x}{:016x}", word(), word())
}

struct State {
    /// When ter stops the run, in Unix ms.
    ends_at: u64,
    /// The `ended` event, once the run is over.
    ended: Option<Value>,
    listeners: usize,
    /// Since when nobody listens.
    alone_since: Instant,
    /// The run as posted, once it is.
    done: Option<Box<Done>>,
}

/// A run that is going on, or has been.
pub struct Live {
    pub id: String,
    /// sim86's own id for the run.
    pub sim86_run: String,
    /// When the board started, in Unix ms.
    pub started_at: u64,
    helper: sim86::Helper,
    state: Mutex<State>,
    events: broadcast::Sender<LiveEvent>,
    stop: mpsc::UnboundedSender<&'static str>,
    over: watch::Receiver<bool>,
}

impl Live {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ends_at(&self) -> u64 {
        self.state().ends_at
    }

    pub fn ended(&self) -> bool {
        self.state().ended.is_some()
    }

    /// The address of sim86's own page for the run, signed in as the
    /// learner, while the run is on. The key is in it: it is handed to
    /// the learner's browser and never printed or kept.
    pub fn view(&self) -> Result<String, CliError> {
        if self.ended() {
            return Err(CliError::new(
                "not_found",
                format!("Live run {:?} is over.", self.id),
            ));
        }
        self.helper.view(&self.sim86_run).map_err(|failure| {
            CliError::new(
                "venue_unavailable",
                format!(
                    "sim86's page for the run could not be shown: {}.",
                    failure.message
                ),
            )
        })
    }

    /// What `/run` answers with.
    pub fn answer(&self) -> Value {
        json!({"live": self.id, "sim86_run": self.sim86_run,
               "started_at": self.started_at, "ends_at": self.ends_at()})
    }

    fn send(&self, name: &'static str, data: Value) {
        let _ = self.events.send(LiveEvent { name, data });
    }

    /// The run's events from now on; nothing from before is sent again.
    /// `None` once the run is over: a browser reconnects a closed stream
    /// by itself, and must then be told there is nothing left to follow.
    pub fn listen(self: &Arc<Self>) -> Option<LiveEvents> {
        let (tx, rx) = mpsc::channel(256);
        let mut state = self.state();
        if state.ended.is_some() {
            return None;
        }
        state.listeners += 1;
        let mut from = self.events.subscribe();
        drop(state);
        let live = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tx.closed() => break,
                    got = from.recv() => match got {
                        Ok(event) => {
                            let last = event.name == "ended";
                            if tx.send(event).await.is_err() || last {
                                break;
                            }
                        }
                        // A listener too slow for a busy pin misses some
                        // of it; the recording does not.
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
            let mut state = live.state();
            state.listeners -= 1;
            state.alone_since = Instant::now();
        });
        Some(rx)
    }

    /// Let the run go on `add_s` seconds longer; the new end, in Unix ms.
    pub fn extend(&self, add_s: u64) -> Result<u64, CliError> {
        let mut state = self.state();
        if state.ended.is_some() {
            return Err(CliError::new("ended", "This live run is over."));
        }
        let ends_at = state.ends_at.saturating_add(add_s.saturating_mul(1_000));
        if ends_at.saturating_sub(self.started_at) > LIVE_CAP_S * 1_000 {
            return Err(CliError::new(
                "limit_cap",
                format!(
                    "A live run lasts {} minutes at most, and this would pass that.",
                    LIVE_CAP_S / 60
                ),
            ));
        }
        state.ends_at = ends_at;
        self.send("limit", json!({"ends_at": ends_at}));
        Ok(ends_at)
    }

    /// Stop the run; `reason` is what its `ended` event says.
    pub fn stop(&self, reason: &'static str) {
        let _ = self.stop.send(reason);
    }

    /// Wait until the run is over and its sim86 run stopped.
    pub async fn over(&self) {
        let mut over = self.over.clone();
        let _ = over.wait_for(|over| *over).await;
    }

    fn take_done(&self) -> Option<Box<Done>> {
        self.state().done.take()
    }

    /// Why the run ended and what was said about it, once it is over.
    fn ended_as(&self) -> Option<(String, String)> {
        let state = self.state();
        let ended = state.ended.as_ref()?;
        let text = |k: &str| ended[k].as_str().unwrap_or_default().to_string();
        Some((text("reason"), text("detail")))
    }
}

/// What asking for a live run came to.
pub enum Started {
    /// The board runs. The events are from its start; drop them when
    /// someone else will listen.
    Live(Arc<Live>, LiveEvents),
    /// Nothing runs: the build failed, there is nothing to check, or
    /// sim86 did not start the run. Posted as any run is. (When sim86 has
    /// no simulator free, nothing is posted: that is an error to try
    /// again on.)
    Posted(Box<Done>),
}

async fn posted(built: Built, outcome: crate::record::Outcome) -> Result<Started, CliError> {
    Ok(Started::Posted(Box::new(built.finish(outcome).await?)))
}

/// Start `built` as a live run that ter stops after `limit_s` seconds.
pub async fn start(mut built: Built, limit_s: u64) -> Result<Started, CliError> {
    let Some(LiveParts {
        file,
        text,
        mut venue,
        elf,
        recording,
        ter,
    }) = built.live_parts()
    else {
        let outcome = built.on_venue()?;
        return posted(built, outcome).await;
    };
    let (text, elf, recording) = (text.to_string(), elf.to_path_buf(), recording.to_path_buf());
    let stimuli = file.stimuli();
    // A net as sim86 names it, to the pin as check.yaml and the board do.
    let nets: BTreeMap<String, (String, Option<String>)> = venue
        .plan
        .watch
        .iter()
        .map(|w| {
            (
                w.net.clone(),
                (w.pin.clone(), ter.pins.get(&w.pin).cloned()),
            )
        })
        .collect();

    if let Err(e) = venue.prepare(&elf) {
        return posted(built, run::infra(&e)).await;
    }
    let program = venue.program.clone().unwrap_or(elf);
    let request = sim86::absolute(&recording.join(REQUEST_FILE));
    let asked =
        serde_json::to_string_pretty(&venue.live_request(&stimuli, &program, LAST_RESORT_MS))
            .expect("the request serialises");
    let io = |what: &str, e: std::io::Error| {
        CliError::new(
            "io_error",
            format!("Could not {what} in {}: {e}", recording.display()),
        )
    };
    std::fs::write(&request, asked).map_err(|e| io("write the sim86 request", e))?;
    let errors =
        std::fs::File::create(recording.join(ERROR_LOG)).map_err(|e| io("keep sim86's log", e))?;

    let mut helper = venue
        .helper
        .command(&["live".into(), request.into()], Some(&recording));
    // Its own group: Ctrl-C in the terminal reaches ter only, which then
    // has the helper read the run back and stop it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut helper, 0);
    let mut helper = tokio::process::Command::from(helper);
    helper
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(errors);
    let started_text = ter_telemetry::rfc3339_utc(SystemTime::now());
    let mut child = match helper.spawn() {
        Ok(child) => child,
        Err(e) => {
            let failure = Failure {
                kind: FailureKind::Missing,
                message: format!("could not run {}: {e}", venue.helper.python.display()),
            };
            return posted(built, run::infra(&venue.failure(&failure))).await;
        }
    };
    let stdin = child.stdin.take();
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let mut said = String::new();
    let start_budget = patience("TER_LIVE_START_SECS", START_BUDGET);
    let began = tokio::time::timeout(start_budget, async {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    said.push_str(&line);
                    said.push('\n');
                    match sim86::live_line(&line) {
                        Some(LiveLine::Started { run }) => break Ok(run),
                        Some(LiveLine::Done(Err(failure))) => break Err(failure),
                        _ => {}
                    }
                }
                _ => {
                    break Err(Failure {
                        kind: FailureKind::Helper,
                        message: "it stopped before the run started".into(),
                    });
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(Failure {
            kind: FailureKind::Helper,
            message: format!(
                "sim86 did not start the run within {} s",
                start_budget.as_secs()
            ),
        })
    });
    let sim86_run = match began {
        Ok(run) => run,
        Err(failure) => {
            // A closed pipe tells the helper to stop: a run it is still
            // making is stopped as soon as it exists. Only a helper that
            // does not answer to that is killed.
            drop(stdin);
            let stop_patience = patience("TER_LIVE_STOP_PATIENCE_SECS", STOP_PATIENCE);
            if tokio::time::timeout(stop_patience, child.wait())
                .await
                .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            let _ = std::fs::write(recording.join(OUTPUT_LOG), &said);
            let _ = std::fs::remove_file(recording.join(FIRMWARE));
            if failure.kind == FailureKind::Busy {
                return Err(CliError::new(
                    "no_capacity",
                    "sim86 has no free capacity right now. Try again in a minute.",
                ));
            }
            return posted(built, run::infra(&venue.failure(&failure))).await;
        }
    };

    let started_at = unix_ms();
    let (events, _) = broadcast::channel(4096);
    let (stop, stop_rx) = mpsc::unbounded_channel();
    let (over_tx, over) = watch::channel(false);
    let live = Arc::new(Live {
        id: new_id(),
        sim86_run,
        started_at,
        helper: venue.helper.clone(),
        state: Mutex::new(State {
            ends_at: started_at + limit_s.min(LIVE_CAP_S) * 1_000,
            ended: None,
            listeners: 0,
            alone_since: Instant::now(),
            done: None,
        }),
        events,
        stop,
        over,
    });
    let first = live.listen().expect("a run that has just started");
    tokio::spawn(
        Drive {
            live: live.clone(),
            built: Some(built),
            venue,
            child,
            stdin,
            lines,
            stop_rx,
            text,
            stimuli,
            started_text,
            nets,
            said,
        }
        .run(over_tx),
    );
    Ok(Started::Live(live, first))
}

/// A going run and everything that follows it to its end.
struct Drive {
    live: Arc<Live>,
    /// The run to post, until it is.
    built: Option<Built>,
    venue: Sim86,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    stop_rx: mpsc::UnboundedReceiver<&'static str>,
    /// check.yaml.
    text: String,
    stimuli: Vec<Stimulus>,
    started_text: String,
    nets: BTreeMap<String, (String, Option<String>)>,
    /// What the helper printed, less the pins and the serial lines.
    said: String,
}

/// Ends the run for whoever follows it, whatever happens to the task
/// that drives it: a task that panics ends it as a sim86 error.
struct Ending {
    live: Arc<Live>,
    over: Option<watch::Sender<bool>>,
}

impl Ending {
    fn end(&mut self, reason: &str, detail: &str) {
        let Some(over) = self.over.take() else {
            return;
        };
        eprintln!("Live run {} ended: {reason}", self.live.id);
        let data = json!({"reason": reason, "detail": detail});
        {
            let mut state = self.live.state();
            state.ended = Some(data.clone());
            self.live.send("ended", data);
        }
        let _ = over.send(true);
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        self.end("sim-error", "ter stopped following the run");
    }
}

impl Drive {
    async fn run(mut self, over: watch::Sender<bool>) {
        let mut ending = Ending {
            live: self.live.clone(),
            over: Some(over),
        };
        let stop_patience = patience("TER_LIVE_STOP_PATIENCE_SECS", STOP_PATIENCE);
        let post_patience = patience("TER_LIVE_POST_SECS", POST_PATIENCE);
        // The checks being posted: the site may be slow, and the run is
        // still followed and stopped meanwhile.
        let mut posting: Option<tokio::task::JoinHandle<Value>> = None;
        let began = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut program = ProgramLines::new(&self.venue.plan);
        let mut asked: Option<(&'static str, Instant)> = None;
        let mut sim_reason = None;
        let mut failure = None;
        loop {
            let stop = tokio::select! {
                line = self.lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    match sim86::live_line(&line) {
                        Some(LiveLine::Pin { t_ns, net, level }) => {
                            let (name, pin) = self.nets.get(&net).cloned().unwrap_or_default();
                            self.live.send("pin", json!({
                                "t_ms": t_ns as f64 / 1e6,
                                "pin": pin.unwrap_or_else(|| name.clone()),
                                "name": name, "net": net, "level": level,
                            }));
                        }
                        Some(LiveLine::Serial { t_ms, source, line }) => {
                            let plain = ter_telemetry::strip_ansi(&line);
                            if program.take(t_ms, &source, &line) && !plain.is_empty() {
                                self.live.send("serial", json!({"t_ms": t_ms, "line": plain}));
                            }
                        }
                        other => {
                            self.said.push_str(&line);
                            self.said.push('\n');
                            match other {
                                Some(LiveLine::Recording(answer)) => {
                                    if let Some(mut built) = self.built.take() {
                                        built.ran(began.elapsed(), "on sim86");
                                        posting = Some(self.checks(built, &answer, post_patience));
                                    }
                                }
                                Some(LiveLine::Ended { reason }) => sim_reason = Some(reason),
                                Some(LiveLine::Done(done)) => failure = done.err(),
                                _ => {}
                            }
                        }
                    }
                    None
                }
                checks = async { posting.as_mut().expect("posting").await }, if posting.is_some() => {
                    posting = None;
                    self.live.send("checks", checks.unwrap_or_else(|e| unposted(&e.to_string())));
                    None
                }
                Some(reason) = self.stop_rx.recv() => Some(reason),
                _ = tick.tick() => match asked {
                    Some((_, at)) if at.elapsed() > stop_patience => {
                        let _ = self.child.start_kill();
                        break;
                    }
                    Some(_) => None,
                    None => {
                        let state = self.live.state();
                        if unix_ms() >= state.ends_at {
                            Some("limit")
                        } else if state.listeners == 0 && state.alone_since.elapsed() >= unwatched() {
                            Some("page-gone")
                        } else {
                            None
                        }
                    }
                },
            };
            if let (Some(reason), None) = (stop, asked) {
                asked = Some((reason, Instant::now()));
                // The helper stops the sim86 run at once; a closed pipe
                // tells it the same.
                if let Some(mut stdin) = self.stdin.take() {
                    let _ = stdin.write_all(b"stop\n").await;
                    let _ = stdin.flush().await;
                }
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        let _ = std::fs::write(self.venue.run_dir.join(OUTPUT_LOG), &self.said);
        let _ = std::fs::remove_file(self.venue.run_dir.join(FIRMWARE));

        // Checks still being posted come before the end; the post has a
        // limit of its own.
        if let Some(posting) = posting.take() {
            let checks = posting.await;
            self.live.send(
                "checks",
                checks.unwrap_or_else(|e| unposted(&e.to_string())),
            );
        }
        // A run that ended before the check's time was up is not an
        // attempt: it is dropped unposted.
        drop(self.built.take());
        let (reason, detail) = ended_as(asked.map(|(reason, _)| reason), failure, sim_reason);
        ending.end(reason, &detail);
    }

    /// Judge the run from the recording the helper read back, and post
    /// it: the `checks` event, which is what `/run` answers for the same
    /// run, once the site has it or has not answered within `patience`.
    fn checks(
        &self,
        built: Built,
        answer: &Answer,
        patience: Duration,
    ) -> tokio::task::JoinHandle<Value> {
        let _ = std::fs::write(self.venue.run_dir.join(OUTPUT_LOG), &self.said);
        let recorded = self
            .venue
            .recording(answer, &self.stimuli, self.started_text.clone());
        let outcome = match recorded {
            Ok(rec) => run::judge(&rec, &self.text, &self.venue.run_dir),
            Err(failure) => Ok(run::infra(&self.venue.failure(&failure))),
        };
        let live = self.live.clone();
        tokio::spawn(async move {
            match outcome {
                Ok(outcome) => match tokio::time::timeout(patience, built.finish(outcome)).await {
                    Ok(done) => checks_of(&live, done),
                    Err(_) => json!({"error": {"code": "network_error", "message": format!(
                        "The site did not answer within {} s, so the run may not be recorded there.",
                        patience.as_secs()
                    )}}),
                },
                Err(e) => json!({"error": {"code": e.code, "message": e.message}}),
            }
        })
    }
}

/// The `checks` event of a post that went wrong inside ter.
fn unposted(why: &str) -> Value {
    json!({"error": {"code": "internal", "message": format!("The run could not be posted: {why}")}})
}

/// The `checks` event of a posted run, which is kept for whoever reports it.
fn checks_of(live: &Live, done: Result<Done, CliError>) -> Value {
    match done {
        Ok(done) => {
            eprintln!(
                "Posted {}: build {}, check {}",
                done.answer["name"].as_str().unwrap_or("?"),
                done.answer["build_status"].as_str().unwrap_or("?"),
                done.answer["check_status"].as_str().unwrap_or("?")
            );
            let answer = done.answer.clone();
            live.state().done = Some(Box::new(done));
            answer
        }
        Err(e) => json!({"error": {"code": e.code, "message": e.message}}),
    }
}

/// Why a run ended, as its `ended` event says it: what ter asked for,
/// else what sim86 or the helper said.
fn ended_as(
    asked: Option<&'static str>,
    failure: Option<Failure>,
    sim_reason: Option<String>,
) -> (&'static str, String) {
    if let Some(failure) = failure {
        return ("sim-error", failure.explain());
    }
    if let Some(reason) = asked {
        return (reason, String::new());
    }
    match sim_reason {
        // Stopped from sim86's own page or tools.
        Some(reason) if reason == "user-stop" => ("user-stop", "stopped on sim86".into()),
        Some(reason) if reason.contains("exit") => ("firmware-exit", reason),
        // The helper lost its connection to the run.
        Some(reason) if reason == "disconnected" => {
            ("sim-error", "the connection to sim86 was lost".into())
        }
        Some(reason)
            if ["realtime", "duration", "limit"]
                .iter()
                .any(|w| reason.contains(w)) =>
        {
            ("limit", reason)
        }
        Some(reason) => {
            let said = reason.strip_prefix("sim-exception:").unwrap_or(&reason);
            ("sim-error", said.trim().to_string())
        }
        None => (
            "sim-error",
            "ter's sim86 helper stopped without saying why".into(),
        ),
    }
}

fn print_event(event: &LiveEvent) {
    let t_ms = event.data["t_ms"].as_f64().unwrap_or(0.0);
    match event.name {
        "pin" => println!(
            "{t_ms:>11.3} ms  {} {}",
            event.data["pin"].as_str().unwrap_or("?"),
            if event.data["level"] == 1 {
                "high"
            } else {
                "low"
            }
        ),
        "serial" => println!(
            "{t_ms:>11.3} ms  | {}",
            event.data["line"].as_str().unwrap_or("")
        ),
        "checks" => eprintln!("The check's time is up and the run is judged; the board runs on."),
        "ended" => {
            let detail = event.data["detail"].as_str().unwrap_or("");
            let sep = if detail.is_empty() { "" } else { ": " };
            eprintln!(
                "The run ended ({}{sep}{detail}).",
                event.data["reason"].as_str().unwrap_or("?")
            );
        }
        _ => {}
    }
}

/// `ter run --sim sim86 --live`: print what the board does until Ctrl-C
/// or the limit, then the verdicts.
pub async fn run(args: RunArgs, limit_s: u64, json: bool) -> Result<(), CliError> {
    if !(1..=LIVE_CAP_S).contains(&limit_s) {
        return Err(CliError::new(
            "bad_request",
            format!("--limit is in seconds, from 1 to {LIVE_CAP_S}."),
        ));
    }
    run::only_sim86(&args)?;
    let built = run::build(args, json).await?;
    match start(built, limit_s).await? {
        Started::Posted(done) => run::report(*done, json),
        Started::Live(live, mut events) => {
            if !json {
                eprintln!(
                    "Live on sim86 for {limit_s} s (sim86 run {}). Ctrl-C stops it.",
                    live.sim86_run
                );
            }
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(event) => {
                            if !json {
                                print_event(&event);
                            }
                            if event.name == "ended" {
                                break;
                            }
                        }
                        None => break,
                    },
                    _ = tokio::signal::ctrl_c() => live.stop("user-stop"),
                }
            }
            live.over().await;
            match (live.take_done(), live.ended_as()) {
                (Some(done), _) => run::report(*done, json),
                (None, Some((reason, detail))) if reason == "sim-error" => Err(CliError::new(
                    "venue_unavailable",
                    format!(
                        "sim86 ended the run before the checks finished ({detail}). Nothing was scored or posted."
                    ),
                )),
                (None, _) => {
                    let said = "Stopped before the checks finished: nothing was scored or posted.";
                    if json {
                        crate::output::print_json(&json!({"posted": false, "message": said}));
                    } else {
                        println!("{said}");
                    }
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_ends_for_ters_reason_else_sim86s() {
        let failure = Failure {
            kind: FailureKind::Api,
            message: "boom".into(),
        };
        assert_eq!(
            ended_as(Some("limit"), None, Some("stopped".into())).0,
            "limit"
        );
        assert_eq!(ended_as(Some("page-gone"), None, None).0, "page-gone");
        let (reason, detail) = ended_as(Some("limit"), Some(failure), None);
        assert_eq!(reason, "sim-error");
        assert!(detail.contains("boom"), "{detail}");
        assert_eq!(
            ended_as(None, None, Some("user-stop".into())).0,
            "user-stop"
        );
        assert_eq!(
            ended_as(None, None, Some("firmware-exit".into())),
            ("firmware-exit", "firmware-exit".into())
        );
        assert_eq!(ended_as(None, None, Some("realtime".into())).0, "limit");
        assert_eq!(
            ended_as(None, None, Some("node-lost".into())),
            ("sim-error", "node-lost".into())
        );
        assert_eq!(ended_as(None, None, Some("duration".into())).0, "limit");
        assert_eq!(
            ended_as(None, None, Some("sim-exception: bad pin value".into())),
            ("sim-error", "bad pin value".into())
        );
        assert_eq!(ended_as(None, None, None).0, "sim-error");
        assert_eq!(
            ended_as(None, None, Some("disconnected".into())),
            ("sim-error", "the connection to sim86 was lost".into())
        );
    }

    #[tokio::test]
    async fn a_run_whose_task_panics_ends_as_a_sim86_error() {
        let (events, _) = broadcast::channel(16);
        let (stop, _stop_rx) = mpsc::unbounded_channel();
        let (over_tx, over) = watch::channel(false);
        let live = Arc::new(Live {
            id: "l1".into(),
            sim86_run: "r1".into(),
            started_at: unix_ms(),
            helper: sim86::Helper {
                python: "python".into(),
                script: "ter_sim86.py".into(),
                key: None,
            },
            state: Mutex::new(State {
                ends_at: unix_ms() + 60_000,
                ended: None,
                listeners: 0,
                alone_since: Instant::now(),
                done: None,
            }),
            events,
            stop,
            over,
        });
        let mut listening = live.listen().expect("a run that is on");
        let ending = Ending {
            live: live.clone(),
            over: Some(over_tx),
        };
        let task = tokio::spawn(async move {
            let _ending = ending;
            panic!("the run's task fails");
        });
        assert!(task.await.is_err());
        tokio::time::timeout(Duration::from_secs(5), live.over())
            .await
            .expect("the run is over");
        assert_eq!(
            live.ended_as(),
            Some(("sim-error".into(), "ter stopped following the run".into()))
        );
        let event = listening.recv().await.expect("the ended event");
        assert_eq!(event.name, "ended");
        assert!(live.listen().is_none());
    }
}
