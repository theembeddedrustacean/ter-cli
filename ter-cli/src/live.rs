//! A live run: the exercise's program on its sim86 board, shown as it
//! happens instead of played back afterwards.
//!
//! The build is `ter run`'s. Then sim86 runs the board with no time limit
//! of its own and ter holds the limit, since sim86 charges for every
//! minute a board runs: the run ends when its time is up (a minute unless
//! it is extended, ten at most), when it is stopped, when a newer run
//! replaces it, or when nobody has listened to it for a while.
//!
//! The page may pause the board and resume it. The time a board is paused
//! does not count against its limit, which moves on by as much when it
//! resumes; a run may be paused for ten minutes in all, and ends at its
//! limit once it has been.
//!
//! The page may also start the board again from its time 0: the helper
//! stops the sim86 run and starts a new one with the program it already
//! has, so nothing is built again. The live run keeps its id, limit and
//! pause allowance; the new sim86 run is a new attempt, judged and posted
//! from a run folder of its own once its window has run, while the old
//! one's checks, if they were still being posted, are no longer sent.
//!
//! Each part of the board with inputs (what a sensor senses) or events
//! (a button's press) is sent to the page as `controls`, from sim86's
//! catalog, with the value each input starts at: the board file's
//! `config.inputs`, else the catalog's default. The page sets an input or
//! sends an event while the board runs or is paused; ter checks it against
//! the controls first, and says on the stream what was done.
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
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use ter_remote::serve::{LIVE_CAP_S, LiveControl, LiveEvent, LiveEvents};
use ter_telemetry::Venue;
use ter_telemetry::event::Stimulus;
use ter_telemetry::sim86::{
    self, Answer, ERROR_LOG, FIRMWARE, Failure, FailureKind, LiveLine, OUTPUT_LOG, PINS_FILE, Plan,
    ProgramLines, REQUEST_FILE, Sim86,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

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
/// How long a run may be paused, in all.
pub const PAUSE_CAP_S: u64 = 600;
/// How long the helper may take to do what the page asked of the board.
const CONTROL_PATIENCE: Duration = Duration::from_secs(15);
/// sim86's own wall-clock limit, past ter's cap and the pauses it allows:
/// what ends a run whose ter and helper are both gone.
const LAST_RESORT_MS: u64 = (LIVE_CAP_S + PAUSE_CAP_S + 60) * 1_000;

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
    /// sim86's own id for the board's run: a new one when it is reset.
    run: String,
    /// When ter stops the run, in Unix ms.
    ends_at: u64,
    /// The `ended` event, once the run is over.
    ended: Option<Value>,
    listeners: usize,
    /// Since when nobody listens.
    alone_since: Instant,
    /// The run as posted, once it is.
    done: Option<Box<Done>>,
    /// Since when the board is paused, and its `paused` event, for whoever
    /// starts to listen meanwhile.
    paused: Option<(Instant, Value)>,
    /// How long the board was paused before, in all, in ms.
    paused_ms: u64,
    /// The parts the page can change, each input at its value now.
    controls: Vec<Value>,
}

impl State {
    fn new(run: String, ends_at: u64, controls: Vec<Value>) -> Self {
        Self {
            run,
            controls,
            ends_at,
            ended: None,
            listeners: 0,
            alone_since: Instant::now(),
            done: None,
            paused: None,
            paused_ms: 0,
        }
    }

    /// How long the board has been paused, in all, in ms.
    fn paused_for(&self) -> u64 {
        let now = self
            .paused
            .as_ref()
            .map_or(0, |(since, _)| since.elapsed().as_millis());
        self.paused_ms
            .saturating_add(u64::try_from(now).unwrap_or(u64::MAX))
    }

    /// The board runs again: the pause, if any, is over and the end moves
    /// on by as much.
    fn unpause(&mut self) -> bool {
        let Some((since, _)) = self.paused.take() else {
            return false;
        };
        let paused = u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.paused_ms = self.paused_ms.saturating_add(paused);
        self.ends_at = self.ends_at.saturating_add(paused);
        true
    }
}

/// What the page asked of the board, and where its answer goes.
type Asked = (LiveControl, oneshot::Sender<Result<Value, CliError>>);

/// A run that is going on, or has been.
pub struct Live {
    pub id: String,
    /// When the board started, in Unix ms.
    pub started_at: u64,
    helper: sim86::Helper,
    state: Mutex<State>,
    events: broadcast::Sender<LiveEvent>,
    stop: mpsc::UnboundedSender<&'static str>,
    control: mpsc::UnboundedSender<Asked>,
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

    /// sim86's own id for the board's run now.
    pub fn sim86_run(&self) -> String {
        self.state().run.clone()
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
        self.helper.view(&self.sim86_run()).map_err(|failure| {
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
        json!({"live": self.id, "sim86_run": self.sim86_run(),
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
        let _ = tx.try_send(LiveEvent {
            name: "controls",
            data: json!({"parts": state.controls}),
        });
        if let Some((_, paused)) = &state.paused {
            let _ = tx.try_send(LiveEvent {
                name: "paused",
                data: paused.clone(),
            });
        }
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
        // The cap is on the time the board runs: the pauses already moved
        // the end on.
        let running = ends_at
            .saturating_sub(self.started_at)
            .saturating_sub(state.paused_ms);
        if running > LIVE_CAP_S * 1_000 {
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

    /// Do `control` to the board; the answer once sim86 has done it.
    pub async fn control(&self, control: LiveControl) -> Result<Value, CliError> {
        let over = || CliError::new("ended", "This live run is over.");
        if self.ended() {
            return Err(over());
        }
        if let Some(why) = refused(&self.state().controls, &control) {
            return Err(CliError::new("bad_request", why));
        }
        // A reset makes a new sim86 run, which may wait for a simulator.
        let patience = match control {
            LiveControl::Reset => patience("TER_LIVE_START_SECS", START_BUDGET),
            _ => patience("TER_LIVE_CONTROL_SECS", CONTROL_PATIENCE),
        };
        let (tx, rx) = oneshot::channel();
        self.control.send((control, tx)).map_err(|_| over())?;
        match tokio::time::timeout(patience, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(over()),
            Err(_) => Err(CliError::new(
                "timeout",
                format!("sim86 did not do it within {} s.", patience.as_secs()),
            )),
        }
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
    let chip = ter.pins.clone();
    // The board's other wired pins, shown beside the checks' so the page's
    // logic analyzer has every pin the program drives.
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let shown = sim86::wired(&read(&venue.board), &venue.plan);
    let pins_json = Path::new(PINS_FILE)
        .file_name()
        .map(|name| read(&venue.board.with_file_name(name)))
        .unwrap_or_default();

    if let Err(e) = venue.prepare(&elf) {
        return posted(built, run::infra(&e)).await;
    }
    let program = venue.program.clone().unwrap_or(elf);
    let request = sim86::absolute(&recording.join(REQUEST_FILE));
    let asked = serde_json::to_string_pretty(&venue.live_request(
        &stimuli,
        &program,
        LAST_RESORT_MS,
        &shown,
    ))
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
                        Some(LiveLine::Started {
                            run,
                            signals,
                            controls,
                        }) => break Ok((run, signals, controls)),
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
    let (sim86_run, signals, controls) = match began {
        Ok(began) => began,
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

    let nets = page_names(&venue.plan, &shown, &chip, &pins_json, &signals);

    let started_at = unix_ms();
    let (events, _) = broadcast::channel(4096);
    let (stop, stop_rx) = mpsc::unbounded_channel();
    let (control, control_rx) = mpsc::unbounded_channel();
    let (over_tx, over) = watch::channel(false);
    let live = Arc::new(Live {
        id: new_id(),
        started_at,
        helper: venue.helper.clone(),
        state: Mutex::new(State::new(
            sim86_run,
            started_at + limit_s.min(LIVE_CAP_S) * 1_000,
            controls.clone(),
        )),
        events,
        stop,
        control,
        over,
    });
    let first = live.listen().expect("a run that has just started");
    tokio::spawn(
        Drive {
            live: live.clone(),
            built: Some(built),
            spare: None,
            firmware: recording.join(FIRMWARE),
            controls,
            venue,
            child,
            stdin,
            lines,
            stop_rx,
            control_rx,
            asked: BTreeMap::new(),
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
    /// The same run again, kept when the first is posted, for a reset.
    spare: Option<Built>,
    /// The program sim86 was given, in the first run folder: the helper
    /// uploads it again on a reset.
    firmware: std::path::PathBuf,
    /// The parts the page can change, as the board starts.
    controls: Vec<Value>,
    venue: Sim86,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    stop_rx: mpsc::UnboundedReceiver<&'static str>,
    control_rx: mpsc::UnboundedReceiver<Asked>,
    /// What the helper was asked and has not answered yet, by the id it
    /// was asked under.
    asked: BTreeMap<u64, Asked>,
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
        let pause_cap = patience("TER_LIVE_PAUSE_SECS", Duration::from_secs(PAUSE_CAP_S));
        let mut next_id: u64 = 0;
        // The checks being posted: the site may be slow, and the run is
        // still followed and stopped meanwhile.
        let mut posting: Option<tokio::task::JoinHandle<Value>> = None;
        let mut began = Instant::now();
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
                            // A net left out as a pin already shown is not sent.
                            if let Some((name, pin)) = self.nets.get(&net).cloned() {
                                self.live.send("pin", json!({
                                    "t_ms": t_ns as f64 / 1e6,
                                    "pin": pin.unwrap_or_else(|| name.clone()),
                                    "name": name, "net": net, "level": level,
                                }));
                            }
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
                                        self.spare = built
                                            .again()
                                            .inspect_err(|e| eprintln!("A reset of live run {} will not be posted: {}", self.live.id, e.message))
                                            .ok();
                                        built.ran(began.elapsed(), "on sim86");
                                        posting = Some(self.checks(built, &answer, post_patience));
                                    }
                                }
                                Some(LiveLine::Ended { reason }) => sim_reason = Some(reason),
                                Some(LiveLine::Done(done)) => failure = done.err(),
                                Some(LiveLine::Reply { id, answer }) => {
                                    if self.answered(id, answer, pause_cap) {
                                        // A new board from its time 0: what
                                        // the old one did is not its own.
                                        program = ProgramLines::new(&self.venue.plan);
                                        began = Instant::now();
                                        sim_reason = None;
                                        // The old run's post goes on, unsent.
                                        drop(posting.take());
                                    }
                                }
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
                Some((control, reply)) = self.control_rx.recv() => {
                    match (&asked, self.stdin.as_mut()) {
                        (None, Some(stdin)) => {
                            next_id += 1;
                            let what = match &control {
                                LiveControl::Pause => json!({"do": "pause"}),
                                LiveControl::Resume => json!({"do": "resume"}),
                                LiveControl::Reset => json!({"do": "reset"}),
                                LiveControl::Input { part, name, value } => {
                                    json!({"do": "input", "part": part, "name": name, "value": value})
                                }
                                LiveControl::Action { part, action } => {
                                    json!({"do": "action", "part": part, "action": action})
                                }
                            };
                            let mut line = what;
                            line["id"] = json!(next_id);
                            let line = format!("{line}\n");
                            if stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok() {
                                self.asked.insert(next_id, (control, reply));
                            } else {
                                let _ = reply.send(Err(CliError::new("ended", "This live run is over.")));
                            }
                        }
                        _ => {
                            let _ = reply.send(Err(CliError::new("ended", "This live run is ending.")));
                        }
                    }
                    None
                }
                _ = tick.tick() => match asked {
                    Some((_, at)) if at.elapsed() > stop_patience => {
                        let _ = self.child.start_kill();
                        break;
                    }
                    Some(_) => None,
                    None => {
                        let state = self.live.state();
                        let paused_out = state.paused.is_some()
                            && state.paused_for() >= u64::try_from(pause_cap.as_millis()).unwrap_or(u64::MAX);
                        if paused_out || (state.paused.is_none() && unix_ms() >= state.ends_at) {
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
        let _ = std::fs::remove_file(&self.firmware);

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

    /// What the helper answered to what it was asked under `id`: the board
    /// paused, resumed or reset, said on the stream, and the answer to the
    /// page. True when the board is a new one from its time 0.
    fn answered(&mut self, id: u64, answer: Result<Value, String>, pause_cap: Duration) -> bool {
        let Some((control, reply)) = self.asked.remove(&id) else {
            return false;
        };
        let mut reset = false;
        let answer = match answer {
            Ok(line) => {
                let t_ms = line["t_ns"].as_u64().unwrap_or(0) as f64 / 1e6;
                let mut state = self.live.state();
                match control {
                    LiveControl::Pause => {
                        if state.paused.is_none() {
                            let left = u64::try_from(pause_cap.as_millis())
                                .unwrap_or(u64::MAX)
                                .saturating_sub(state.paused_ms);
                            let data = json!({"t_ms": t_ms, "pause_ends_at": unix_ms().saturating_add(left)});
                            state.paused = Some((Instant::now(), data.clone()));
                            self.live.send("paused", data);
                        }
                        let (_, data) = state.paused.as_ref().expect("paused");
                        let mut answer = data.clone();
                        answer["ok"] = json!(true);
                        answer["t_ms"] = json!(t_ms);
                        Ok(answer)
                    }
                    LiveControl::Resume => {
                        if state.unpause() {
                            self.live
                                .send("resumed", json!({"t_ms": t_ms, "ends_at": state.ends_at}));
                            self.live.send("limit", json!({"ends_at": state.ends_at}));
                        }
                        Ok(json!({"ok": true, "t_ms": t_ms, "ends_at": state.ends_at}))
                    }
                    LiveControl::Reset => {
                        reset = true;
                        let run = line["run"].as_str().unwrap_or_default().to_string();
                        state.run = run.clone();
                        // The new board runs from the start, its parts as
                        // they start.
                        state.unpause();
                        state.controls = self.controls.clone();
                        drop(state);
                        self.renew();
                        let data = json!({"run": run, "ends_at": self.live.ends_at()});
                        self.live.send("reset", data.clone());
                        self.live.send("controls", json!({"parts": self.controls}));
                        let mut answer = data;
                        answer["ok"] = json!(true);
                        Ok(answer)
                    }
                    LiveControl::Input { part, name, value } => {
                        let set = state
                            .controls
                            .iter_mut()
                            .filter(|p| p["id"] == part.as_str())
                            .filter_map(|p| p["controls"].as_array_mut())
                            .flatten()
                            .find(|c| c["kind"] == "input" && c["name"] == name.as_str());
                        if let Some(control) = set {
                            control["value"] = value.clone();
                        }
                        let data =
                            json!({"part": part, "name": name, "value": value, "t_ms": t_ms});
                        self.live.send("input", data.clone());
                        let mut answer = data;
                        answer["ok"] = json!(true);
                        Ok(answer)
                    }
                    LiveControl::Action { part, action } => {
                        let data = json!({"part": part, "action": action, "t_ms": t_ms});
                        self.live.send("action", data.clone());
                        let mut answer = data;
                        answer["ok"] = json!(true);
                        Ok(answer)
                    }
                }
            }
            Err(message) => Err(CliError::new(
                "venue_unavailable",
                format!("sim86 did not do it: {message}."),
            )),
        };
        let _ = reply.send(answer);
        reset
    }

    /// The run to post for a board started again: the one not posted yet,
    /// whose window had not run, else the posted one's twin in a run
    /// folder of its own. What the helper said so far stays with the run
    /// it was said of.
    fn renew(&mut self) {
        let _ = std::fs::write(self.venue.run_dir.join(OUTPUT_LOG), &self.said);
        self.said.clear();
        if self.built.is_some() {
            return;
        }
        let Some(mut built) = self.spare.take() else {
            return;
        };
        match built.afresh() {
            Ok(folder) => {
                self.venue.run_dir = folder.to_path_buf();
                self.built = Some(built);
            }
            Err(e) => eprintln!(
                "The reset of live run {} will not be posted: {}",
                self.live.id, e.message
            ),
        }
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

/// A net as sim86 names it, to `(name, pin)`: the name check.yaml or the
/// board gives it, and the chip's name for it when ter has one, which is
/// what the page keys its levels and rows by.
///
/// The nets `plan` watches keep their check names and ter.toml's chip
/// names. Each net in `shown` is a pin of the programmed part, named by
/// its chip signal: ter.toml's name for a pins.json entry on that net,
/// else sim86's (`signals`), else the board's own pin name. A net whose
/// pin is already there under another net or name is left out, so no pin
/// is sent twice.
fn page_names(
    plan: &Plan,
    shown: &[String],
    chip: &BTreeMap<String, String>,
    pins_json: &str,
    signals: &BTreeMap<String, String>,
) -> BTreeMap<String, (String, Option<String>)> {
    // ter.toml's chip names, by the net pins.json puts each name on.
    let by_net: BTreeMap<String, String> =
        serde_json::from_str::<BTreeMap<String, Value>>(pins_json)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, net)| Some((net.as_str()?.to_string(), chip.get(&name)?.clone())))
            .collect();
    let board_pin = |net: &str| -> Option<String> {
        net.strip_prefix(plan.mcu.as_str())?
            .strip_prefix('.')
            .map(str::to_string)
    };
    // The chip signal a pin of the programmed part is, when ter knows it.
    let signal = |net: &str, pin: &str| by_net.get(net).or_else(|| signals.get(pin)).cloned();

    let mut nets: BTreeMap<String, (String, Option<String>)> = plan
        .watch
        .iter()
        .map(|w| (w.net.clone(), (w.pin.clone(), chip.get(&w.pin).cloned())))
        .collect();
    // Every name a pin is already sent or known under.
    let mut taken: Vec<String> = Vec::new();
    for (net, (name, pin)) in &nets {
        taken.push(pin.clone().unwrap_or_else(|| name.clone()));
        if let Some(on) = board_pin(net) {
            taken.extend(signal(net, &on));
            taken.push(on);
        }
    }
    for net in shown {
        let Some(on) = board_pin(net) else { continue };
        let signal = signal(net, &on);
        let page = signal.clone().unwrap_or_else(|| on.clone());
        if nets.contains_key(net) || taken.contains(&page) || taken.contains(&on) {
            continue;
        }
        taken.push(page);
        taken.push(on.clone());
        nets.insert(net.clone(), (on, signal));
    }
    nets
}

/// Why the page may not ask `control` of a board whose parts are
/// `parts`, as the `controls` event gives them; `None` when it may.
fn refused(parts: &[Value], control: &LiveControl) -> Option<String> {
    let (part, kind, what) = match control {
        LiveControl::Input { part, .. } => (part, "input", "an input"),
        LiveControl::Action { part, .. } => (part, "action", "events"),
        _ => return None,
    };
    let of = |p: &Value| -> Vec<Value> {
        p["controls"]
            .as_array()
            .map(|c| c.iter().filter(|c| c["kind"] == kind).cloned().collect())
            .unwrap_or_default()
    };
    let named = |v: &[Value], key: &str| -> String {
        v.iter()
            .filter_map(|c| c[key].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let takes: Vec<Value> = parts
        .iter()
        .filter(|p| !of(p).is_empty())
        .cloned()
        .collect();
    let Some(found) = takes.iter().find(|p| p["id"] == part.as_str()) else {
        return Some(if takes.is_empty() {
            format!("No part of this board takes {what}.")
        } else {
            format!(
                "No part {part:?} of this board takes {what}; these do: {}.",
                named(&takes, "id")
            )
        });
    };
    let controls = of(found);
    match control {
        LiveControl::Input { name, value, .. } => {
            let Some(spec) = controls.iter().find(|c| c["name"] == name.as_str()) else {
                return Some(format!(
                    "{part} has no input {name:?}; it has {}.",
                    named(&controls, "name")
                ));
            };
            let axes = spec["axes"]
                .as_array()
                .map(Vec::len)
                .or_else(|| spec["axes"].as_u64().and_then(|n| usize::try_from(n).ok()));
            let values: Vec<f64> = match (value, axes) {
                (Value::Array(list), Some(n)) if list.len() == n => {
                    list.iter().filter_map(Value::as_f64).collect()
                }
                (Value::Number(n), None) => n.as_f64().into_iter().collect(),
                (_, Some(n)) => return Some(format!("{name} is a list of {n} numbers.")),
                _ => return Some(format!("{name} is one number.")),
            };
            let unit = spec["unit"].as_str().filter(|u| !u.is_empty());
            let unit = unit.map(|u| format!(" {u}")).unwrap_or_default();
            let (min, max) = (spec["min"].as_f64(), spec["max"].as_f64());
            let out = |v: &f64| min.is_some_and(|m| *v < m) || max.is_some_and(|m| *v > m);
            if values.iter().any(out) {
                let bound = |b: Option<f64>| b.map_or("any".to_string(), |b| b.to_string());
                return Some(format!(
                    "{name} is from {} to {}{unit}.",
                    bound(min),
                    bound(max)
                ));
            }
            None
        }
        LiveControl::Action { action, .. } => {
            let actions: Vec<&str> = controls
                .iter()
                .filter_map(|c| c["actions"].as_array())
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            (!actions.contains(&action.as_str())).then(|| {
                format!(
                    "{part} takes the events {}; not {action:?}.",
                    actions.join(", ")
                )
            })
        }
        _ => None,
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
        "reset" => eprintln!(
            "The board starts again from 0 (sim86 run {}).",
            event.data["run"].as_str().unwrap_or("?")
        ),
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
                    live.sim86_run()
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

    fn plan_of(watch: &[(&str, &str)]) -> Plan {
        Plan {
            mcu: "mcu".into(),
            watch: watch
                .iter()
                .map(|(pin, net)| sim86::Watch {
                    pin: pin.to_string(),
                    net: net.to_string(),
                })
                .collect(),
            buttons: BTreeMap::new(),
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn nets(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// What the page keys each net's events by.
    fn keyed(names: &BTreeMap<String, (String, Option<String>)>) -> Vec<(String, String)> {
        names
            .iter()
            .map(|(net, (name, pin))| (net.clone(), pin.clone().unwrap_or_else(|| name.clone())))
            .collect()
    }

    #[test]
    fn a_serial_only_run_shows_the_wired_pins_by_their_chip_names() {
        // The voltmeter: four LEDs, checks on serial only.
        let shown = nets(&["mcu.D12", "mcu.D11", "mcu.D7", "mcu.D8"]);
        let signals = map(&[("D12", "PA6"), ("D11", "PA7"), ("D7", "PA8"), ("D8", "PA9")]);
        let names = page_names(&plan_of(&[]), &shown, &BTreeMap::new(), "{}", &signals);
        assert_eq!(
            names["mcu.D12"],
            ("D12".to_string(), Some("PA6".to_string()))
        );
        assert_eq!(
            keyed(&names),
            [
                ("mcu.D11".into(), "PA7".into()),
                ("mcu.D12".into(), "PA6".into()),
                ("mcu.D7".into(), "PA8".into()),
                ("mcu.D8".into(), "PA9".into()),
            ]
        );
        // Without sim86's catalog: ter.toml's name through pins.json, else
        // the board's.
        let names = page_names(
            &plan_of(&[]),
            &shown,
            &map(&[("buzzer", "PA9")]),
            r#"{"buzzer": "mcu.D8", "adc_a0": "mcu.A0", "user_button": {"part": "mcu", "input": "button"}}"#,
            &BTreeMap::new(),
        );
        assert_eq!(names["mcu.D8"].1.as_deref(), Some("PA9"));
        assert_eq!(names["mcu.D12"], ("D12".to_string(), None));
    }

    #[test]
    fn a_watched_pin_keeps_its_name_and_is_not_sent_twice() {
        let plan = plan_of(&[("user_led", "mcu.D13")]);
        let chip = map(&[("user_led", "PA5")]);
        // PA5 is D13 under its other name on the board; D12 is new.
        let shown = nets(&["mcu.PA5", "mcu.D12", "mcu.PA6"]);
        let signals = map(&[
            ("D13", "PA5"),
            ("PA5", "PA5"),
            ("D12", "PA6"),
            ("PA6", "PA6"),
        ]);
        let names = page_names(&plan, &shown, &chip, r#"{"user_led": "mcu.D13"}"#, &signals);
        assert_eq!(
            keyed(&names),
            [
                ("mcu.D12".into(), "PA6".into()),
                ("mcu.D13".into(), "PA5".into()),
            ]
        );
        assert_eq!(names["mcu.D13"].0, "user_led");
        // Without a chip name the watched pin keeps its check name, and
        // sim86's signal still finds its twin.
        let names = page_names(&plan, &shown, &BTreeMap::new(), "{}", &signals);
        assert_eq!(names["mcu.D13"], ("user_led".to_string(), None));
        assert!(!names.contains_key("mcu.PA5"), "{names:?}");
        // With nothing to tell them apart, a board name is still sent once.
        let names = page_names(
            &plan_of(&[("D12", "led1.+")]),
            &nets(&["mcu.D12", "mcu.D12"]),
            &BTreeMap::new(),
            "{}",
            &BTreeMap::new(),
        );
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(names["led1.+"].0, "D12");
    }

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

    #[test]
    fn an_input_or_an_event_is_checked_against_the_boards_controls() {
        let parts = vec![
            json!({"id": "ldr1", "label": "Photoresistor (LDR)", "type": "Ldr", "controls": [
                {"kind": "input", "name": "lux", "unit": "lux", "min": 0.1, "max": 10000.0, "value": 100.0}]}),
            json!({"id": "imu", "label": "IMU", "type": "Imu", "controls": [
                {"kind": "input", "name": "acceleration", "axes": ["x", "y", "z"], "value": [0, 0, 9.8]}]}),
            json!({"id": "btn1", "label": "Button", "type": "Btn", "controls": [
                {"kind": "action", "name": "events", "actions": ["pressed", "released"]}]}),
        ];
        let input = |part: &str, name: &str, value: Value| LiveControl::Input {
            part: part.into(),
            name: name.into(),
            value,
        };
        let action = |part: &str, action: &str| LiveControl::Action {
            part: part.into(),
            action: action.into(),
        };
        for ok in [
            input("ldr1", "lux", json!(500)),
            input("ldr1", "lux", json!(0.1)),
            input("imu", "acceleration", json!([0, 9.8, 0])),
            action("btn1", "released"),
            LiveControl::Pause,
        ] {
            assert_eq!(refused(&parts, &ok), None, "{ok:?}");
        }
        for (bad, said) in [
            (
                input("pot1", "position", json!(1)),
                "No part \"pot1\" of this board takes an input; these do: ldr1, imu.",
            ),
            (
                input("btn1", "lux", json!(1)),
                "No part \"btn1\" of this board takes an input; these do: ldr1, imu.",
            ),
            (
                input("ldr1", "light", json!(1)),
                "ldr1 has no input \"light\"; it has lux.",
            ),
            (
                input("ldr1", "lux", json!(20000)),
                "lux is from 0.1 to 10000 lux.",
            ),
            (input("ldr1", "lux", json!([1])), "lux is one number."),
            (
                input("imu", "acceleration", json!(1)),
                "acceleration is a list of 3 numbers.",
            ),
            (
                action("ldr1", "pressed"),
                "No part \"ldr1\" of this board takes events; these do: btn1.",
            ),
            (
                action("btn1", "click"),
                "btn1 takes the events pressed, released; not \"click\".",
            ),
        ] {
            assert_eq!(refused(&parts, &bad).as_deref(), Some(said), "{bad:?}");
        }
        assert_eq!(
            refused(&[], &action("btn1", "pressed")).as_deref(),
            Some("No part of this board takes events.")
        );
    }

    #[tokio::test]
    async fn a_run_whose_task_panics_ends_as_a_sim86_error() {
        let (events, _) = broadcast::channel(16);
        let (stop, _stop_rx) = mpsc::unbounded_channel();
        let (control, _control_rx) = mpsc::unbounded_channel();
        let (over_tx, over) = watch::channel(false);
        let live = Arc::new(Live {
            id: "l1".into(),
            started_at: unix_ms(),
            helper: sim86::Helper {
                python: "python".into(),
                script: "ter_sim86.py".into(),
                key: None,
            },
            state: Mutex::new(State::new("r1".into(), unix_ms() + 60_000, Vec::new())),
            events,
            stop,
            control,
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
        let event = listening.recv().await.expect("the controls event");
        assert_eq!(event.name, "controls");
        let event = listening.recv().await.expect("the ended event");
        assert_eq!(event.name, "ended");
        assert!(live.listen().is_none());
    }
}
