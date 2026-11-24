//! `ter serve`: the lesson page's editor runs exercises through this
//! machine's ter.
//!
//! A `/run` is `ter run` on the exercise's folder (fetched first if it is
//! not on this machine), with the learner's `src/` files from the page
//! written into it first, posted with this machine's token. The answer is
//! what `ter run --json` prints: the run as posted, `name`, `site`. A
//! failed build or check is still a posted run, so it answers 200; only a
//! run that was not posted is an error status.
//!
//! A hardware run may name its venue: `local` (this machine's board) or
//! `bench` (a shared bench this account connected to with `ter connect`,
//! the newest unless the page names one). A bench run goes through the
//! same bench client as `ter run --hw --venue bench`, and waits a little
//! while someone else is driving before answering `bench_busy`.
//!
//! A simulation may name its simulator the same way (`wokwi`, `sim86`):
//! that run goes there or is refused before the build, never to another
//! simulator. `/health` lists the simulators and whether this machine is
//! set up for each.
//!
//! A run on sim86 may be asked for live (`"live"` in `/run`): the answer
//! then names a live run ([`crate::live`]) instead of carrying a posted
//! one, and the page follows it on `/live/<id>/events`. A failed build,
//! or a run sim86 did not start, answers as any `/run` does. There is one
//! live run at a time: any new `/run` stops the one before.
//!
//! One run at a time: a second request waits for the first, since both
//! would build in the same course cache. A live run holds that turn only
//! until its board runs.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ter_check::CheckFile;
use ter_remote::serve::{
    LiveEvents, RunFuture, RunRequest, ServeError, Service, editable, no_live, origin_of,
};
use ter_sdk::project::TerToml;
use ter_telemetry::sim86::Sim86;

use crate::config::Config;
use crate::exercises;
use crate::live::{self, Live, Started};
use crate::output::CliError;
use crate::remote;
use crate::run::{self, CHECK_YAML, RunArgs};
use crate::session::Session;
use crate::share;

/// How long a build may take: the first build of a course compiles the HAL
/// (and, for ESP-IDF, the IDF) from nothing.
pub const BUILD_ALLOWANCE: Duration = Duration::from_secs(600);
/// A board's flash, on top of its capture.
const FLASH_ALLOWANCE: Duration = Duration::from_secs(180);
/// How long a page's bench run waits while someone else drives the bench.
pub const BENCH_QUEUE: Duration = Duration::from_secs(30);
/// A bench run's trip to the owner's machine and back, on top of the
/// flash and the capture.
const BENCH_ALLOWANCE: Duration = Duration::from_secs(60);
/// The places a page's run can go, as `/health` lists them.
pub const VENUES: [&str; 3] = ["simulation", "local", "bench"];

/// How long `/health` reuses what it found about the simulators: finding
/// out starts sim86's Python.
const SIMULATORS_FOR: Duration = Duration::from_secs(20);

struct Inner {
    /// Held for the whole of a run, past a timeout answer too.
    turn: Arc<tokio::sync::Mutex<()>>,
    /// The simulators as `/health` last listed them, and when.
    simulators: std::sync::Mutex<Option<(std::time::Instant, Value)>>,
    /// The live run, going or over, until the next one.
    live: std::sync::Mutex<Option<Arc<Live>>>,
}

impl Inner {
    fn live(&self, id: &str) -> Option<Arc<Live>> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|live| live.id == id)
    }
}

/// One simulator as `/health` lists it: `ready` when this machine has what
/// a run on it needs, else why not, in words for the learner.
fn simulator(name: &str, setup: Result<(), CliError>) -> Value {
    match setup {
        Ok(()) => json!({"name": name, "ready": true}),
        Err(e) => json!({"name": name, "ready": false, "reason": e.message}),
    }
}

/// Every simulator a page can name, in the order of [`run::SIM_VENUES`]:
/// Wokwi, the default, first.
fn simulators() -> Value {
    Value::Array(
        run::SIM_VENUES
            .iter()
            .map(|name| {
                let setup = match *name {
                    "sim86" => crate::sim86::setup(),
                    _ => crate::sim::setup().map(|_| ()),
                };
                simulator(name, setup)
            })
            .collect(),
    )
}

pub struct Serve(Arc<Inner>);

/// A run that was not posted, as `ter serve` answers it.
struct ServeFailure(ServeError);

impl From<CliError> for ServeFailure {
    fn from(e: CliError) -> Self {
        ServeFailure(ServeError::new(&e.code, e.message))
    }
}

impl Service for Serve {
    fn health(&self) -> Value {
        let posts = Session::open().is_ok_and(|s| s.token_source != "none");
        let simulators = {
            let mut kept = self.0.simulators.lock().unwrap_or_else(|e| e.into_inner());
            match kept.as_ref() {
                Some((at, list)) if at.elapsed() < SIMULATORS_FOR => list.clone(),
                _ => {
                    let list = simulators();
                    *kept = Some((std::time::Instant::now(), list.clone()));
                    list
                }
            }
        };
        json!({"version": env!("CARGO_PKG_VERSION"), "posts_to_site": posts,
               "venues": VENUES, "simulators": simulators, "live": true})
    }

    fn live_events(&self, id: &str) -> Option<LiveEvents> {
        self.0.live(id).and_then(|live| live.listen())
    }

    fn live_extend(&self, id: &str, add_s: u64) -> Result<Value, ServeError> {
        let live = self.0.live(id).ok_or_else(|| no_live(id))?;
        let ends_at = live
            .extend(add_s)
            .map_err(|e| ServeError::new(&e.code, e.message))?;
        Ok(json!({"ends_at": ends_at}))
    }

    fn live_stop(&self, id: &str) -> Result<Value, ServeError> {
        let live = self.0.live(id).ok_or_else(|| no_live(id))?;
        live.stop("user-stop");
        Ok(json!({"ok": true}))
    }

    fn run(&self, request: RunRequest) -> RunFuture {
        let inner = self.0.clone();
        Box::pin(async move { handle(inner, request).await.map_err(|f| f.0) })
    }
}

/// How long a new run waits for the live run before it to stop.
const REPLACE_PATIENCE: Duration = Duration::from_secs(20);
/// What starting a live run adds to a build: the upload to sim86 and the
/// wait for a free simulator.
const LIVE_START_ALLOWANCE: Duration = Duration::from_secs(480);

async fn handle(inner: Arc<Inner>, request: RunRequest) -> Result<Value, ServeFailure> {
    // One live run at a time, and none left running under another run.
    let before = inner.live.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(before) = before.filter(|live| !live.ended()) {
        before.stop("replaced");
        let _ = tokio::time::timeout(REPLACE_PATIENCE, before.over()).await;
    }
    let turn = inner.turn.clone().lock_owned().await;
    let session = Session::open()?;
    let user = session
        .client
        .ping()
        .await
        .map_err(|e| session.forget_dead_token(e.into()))?
        .user
        .clone();
    if let Some(page_user) = &request.user
        && page_user != &user
    {
        return Err(CliError::new(
            "wrong_account",
            format!(
                "ter on this machine is paired with {user}, and the page is signed in as {page_user}. Sign in to the site as {user}, or pair ter with your own account (`ter login`)."
            ),
        )
        .into());
    }

    let dir = match exercises::fetched_dir(&request.exercise_id)? {
        Some(dir) => dir,
        None => {
            eprintln!("Fetching {}", request.exercise_id);
            exercises::fetch_into_courses(&session, &request.exercise_id)
                .await
                .map_err(|e| session.forget_dead_token(e))?
        }
    };
    let not_written = write_files(&dir, &request)?;
    let venue = request.venue.clone();
    let mode = mode_for(&dir, request.mode.as_deref());
    let limit = limit(&dir, mode, venue.as_deref());
    let on = match (&venue, &request.bench) {
        (Some(v), Some(b)) => format!(", {v} {b}"),
        (Some(v), None) => format!(", {v}"),
        _ => String::new(),
    };
    eprintln!(
        "Running {} ({mode}{on}) in {}",
        request.exercise_id,
        dir.display()
    );
    let bench = remote::Want {
        bench: request.bench.clone(),
        wait: bench_queue(),
    };
    let args = RunArgs {
        dir: Some(dir),
        mode: Some(mode),
        venue,
        bench,
        no_check: false,
    };
    let live_limit = request.live;
    if live_limit.is_some() {
        run::only_sim86(&args)?;
    }
    let limit = match live_limit {
        Some(_) => BUILD_ALLOWANCE + LIVE_START_ALLOWANCE,
        None => limit,
    };

    // The run goes on in its own task, holding the turn, so a timeout
    // answers the page without leaving a second build to start under it.
    let task = tokio::spawn(async move {
        let done = match live_limit {
            None => run::execute(args, true)
                .await
                .map(|done| Ok(Box::new(done))),
            Some(limit_s) => match run::build(args, true).await {
                Ok(built) => live::start(built, limit_s)
                    .await
                    .map(|started| match started {
                        Started::Posted(done) => Ok(done),
                        // The page listens from here on; with no listener the
                        // run stops by itself.
                        Started::Live(live, _events) => {
                            let answer = live.answer();
                            *inner.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(live);
                            Err(answer)
                        }
                    }),
                Err(e) => Err(e),
            },
        };
        drop(turn);
        done
    });
    let done = match tokio::time::timeout(limit, task).await {
        Ok(Ok(done)) => done?,
        Ok(Err(e)) => {
            return Err(CliError::new("server_error", format!("The run stopped: {e}")).into());
        }
        Err(_) => {
            return Err(CliError::new(
                "timeout",
                format!(
                    "The run took longer than {} s. It carries on in ter serve and may still be posted.",
                    limit.as_secs()
                ),
            )
            .into());
        }
    };
    let done = match done {
        Ok(done) => done,
        Err(mut live) => {
            if !not_written.is_empty() {
                live["not_written"] = json!(not_written);
            }
            eprintln!(
                "Live run {} started on sim86",
                live["live"].as_str().unwrap_or("?")
            );
            return Ok(live);
        }
    };
    let mut answer = done.answer;
    if !not_written.is_empty() {
        answer["not_written"] = json!(not_written);
    }
    eprintln!(
        "Posted {}: build {}, check {}",
        answer["name"].as_str().unwrap_or("?"),
        answer["build_status"].as_str().unwrap_or("?"),
        answer["check_status"].as_str().unwrap_or("?")
    );
    Ok(answer)
}

/// Write the page's `src/` files into `dir`, and return the other files
/// the page sent that differ from the folder's: those are never taken from
/// the browser (they build on this machine, or judge the run).
fn write_files(dir: &Path, request: &RunRequest) -> Result<Vec<String>, CliError> {
    let io = |path: &Path, e: std::io::Error| {
        CliError::new(
            "io_error",
            format!("Could not write {}: {e}", path.display()),
        )
    };
    let mut not_written = Vec::new();
    for file in &request.files {
        let path = dir.join(&file.path);
        let on_disk = std::fs::read(&path).ok();
        if on_disk.as_deref() == Some(file.content.as_bytes()) {
            continue;
        }
        // ter adds its own lines to .gitignore at fetch, so the page's
        // copy differs by design; it neither builds nor judges.
        if file.path == ter_sdk::project::GITIGNORE {
            continue;
        }
        if !editable(&file.path) {
            not_written.push(file.path.clone());
            continue;
        }
        // Every part below the exercise must be a real folder, not a link
        // out of it.
        let mut at = dir.to_path_buf();
        for part in Path::new(&file.path).iter() {
            at.push(part);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(CliError::new(
                    "bad_request",
                    format!(
                        "{} is a link; ter serve only writes inside the exercise.",
                        at.display()
                    ),
                ));
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::write(&path, &file.content).map_err(|e| io(&path, e))?;
    }
    Ok(not_written)
}

/// How long a bench run waits for another driver: [`BENCH_QUEUE`], or
/// `TER_BENCH_QUEUE_SECS` when set (tests set it short).
fn bench_queue() -> Duration {
    std::env::var("TER_BENCH_QUEUE_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map_or(BENCH_QUEUE, Duration::from_secs)
}

/// The mode of a page's run. A page that names none gets a simulation
/// where the exercise has one (the editor's Run button is the simulation
/// path), and a run on the board where the board is all it allows.
fn mode_for(dir: &Path, asked: Option<&str>) -> &'static str {
    match asked {
        Some("hardware") => "hardware",
        Some(_) => "simulation",
        None => match TerToml::load(dir) {
            Ok(ter)
                if !crate::sim::offered(dir, &ter.modes)
                    && ter.modes.iter().any(|m| m == "hardware") =>
            {
                "hardware"
            }
            _ => "simulation",
        },
    }
}

/// The most a run may take: the build, then the venue's own budget for
/// the check's `timeout_ms` (the named simulator's, or the one the
/// exercise runs on when none is named). A bench run also has its queue
/// and the trip to the owner's machine.
fn limit(dir: &Path, mode: &str, venue: Option<&str>) -> Duration {
    let timeout_ms = std::fs::read_to_string(dir.join(CHECK_YAML))
        .ok()
        .and_then(|t| CheckFile::parse(&t).ok())
        .map_or(0, |f| f.timeout_ms);
    let run = if mode != "hardware" {
        match venue {
            Some("sim86") => Sim86::budget(timeout_ms),
            Some(_) => run::wokwi_budget(timeout_ms),
            None => run::sim_budget(dir, timeout_ms),
        }
    } else if venue == Some("bench") {
        bench_queue() + BENCH_ALLOWANCE + FLASH_ALLOWANCE + run::board_budget(timeout_ms)
    } else {
        FLASH_ALLOWANCE + run::board_budget(timeout_ms)
    };
    BUILD_ALLOWANCE + run
}

/// `--share` and its settings, before the ports are settled.
pub struct ShareFlags {
    pub board: String,
    pub label: Option<String>,
    pub url: Option<String>,
    pub bind: std::net::IpAddr,
    pub bench_port: Option<u16>,
}

pub async fn serve(
    port: Option<u16>,
    dev: bool,
    share: Option<ShareFlags>,
    json: bool,
) -> Result<(), CliError> {
    let config = Config::load()?;
    let port = port.unwrap_or(config.serve_port);
    // The lesson page is on the site this ter posts its runs to.
    let page = origin_of(&config.site_url).ok_or_else(|| {
        CliError::new(
            "config_error",
            format!(
                "The site address {:?} is not an http or https address, so ter serve cannot tell which lesson page to answer.",
                config.site_url
            ),
        )
    })?;
    let elsewhere = origin_of(ter_sdk::DEFAULT_SITE_URL).is_some_and(|live| live != page);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| {
            CliError::new(
                "io_error",
                format!("Could not listen on 127.0.0.1:{port}: {e}. Is another ter serve running?"),
            )
        })?;
    let bound = listener
        .local_addr()
        .map_err(|e| CliError::new("io_error", e.to_string()))?
        .port();
    let who = match Session::open() {
        Ok(s) if s.token_source != "none" => match s.client.ping().await {
            Ok(p) => format!("posting runs as {}", p.user),
            Err(e) => format!("the site refused this machine's token ({})", e.code()),
        },
        _ => "not paired: runs are refused until `ter login`".into(),
    };
    let turn = Arc::new(tokio::sync::Mutex::new(()));
    let sharing = match share {
        None => None,
        Some(flags) => {
            let session = Session::open()?;
            let args = share::ShareArgs {
                board: flags.board,
                label: flags.label,
                url: flags.url,
                bind: flags.bind,
                bench_port: flags
                    .bench_port
                    .unwrap_or_else(|| share::default_bench_port(port)),
            };
            Some(
                share::start(session, args, turn.clone())
                    .await
                    .map_err(|e| {
                        let mut e = e;
                        e.message = format!("Could not share the bench: {}", e.message);
                        e
                    })?,
            )
        }
    };

    let url = format!("http://127.0.0.1:{bound}");
    let banner = if json {
        let bench = sharing.as_ref().map(|s| {
            json!({"bench": s.bench, "label": s.label, "board": s.board,
                   "share_code": s.code, "url": s.url, "listen": s.listen})
        });
        format!(
            "{}\n",
            json!({"url": url, "port": bound, "page": page, "dev": dev, "bench": bench})
        )
    } else {
        let mut b = format!(
            "ter serve on {url}, {who}\nThe lesson page's Run button builds here. Stop with Ctrl-C.\n"
        );
        if elsewhere {
            b.push_str(&format!(
                "This ter is set to the site {}: only a lesson page on {page} can start runs here, not one on {}.\n",
                config.site_url,
                ter_sdk::DEFAULT_SITE_URL
            ));
        }
        if dev {
            b.push_str(&format!(
                "--dev: a lesson page on {} can start runs here too.\n",
                ter_remote::serve::DEV_ORIGIN
            ));
        }
        if let Some(s) = &sharing {
            b.push_str(&format!(
                "Bench {} ({}, {}) is shared as {}: others run `ter connect {}`. Runs reach it at {} (listening on {}).\nType k and Enter to drop whoever is driving and stop sharing.\n",
                s.label, s.board, s.bench, s.code, s.code, s.url, s.listen
            ));
        }
        b
    };
    // One write, flushed so a caller reading the URL sees it now, and never
    // a panic if whoever started ter serve stopped reading its output.
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(banner.as_bytes()).and_then(|()| out.flush());
    }

    let service = Arc::new(Serve(Arc::new(Inner {
        turn,
        simulators: std::sync::Mutex::new(None),
        live: std::sync::Mutex::new(None),
    })));
    ter_remote::serve::serve(listener, service, &page, dev)
        .await
        .map_err(|e| CliError::new("io_error", format!("ter serve stopped: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ter_remote::serve::PageFile;

    fn request(files: &[(&str, &str)]) -> RunRequest {
        RunRequest {
            exercise_id: "x".into(),
            files: files
                .iter()
                .map(|(p, c)| PageFile {
                    path: p.to_string(),
                    content: c.to_string(),
                })
                .collect(),
            mode: Some("simulation".into()),
            venue: None,
            bench: None,
            user: None,
            live: None,
        }
    }

    #[test]
    fn only_src_is_written_and_the_rest_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(d.join("check.yaml"), "timeout_ms: 1\n").unwrap();
        let not = write_files(
            d,
            &request(&[
                ("src/bin/main.rs", "fn main() {}\n"),
                ("Cargo.toml", "[package]\n"),
                ("check.yaml", "timeout_ms: 1\nassert: []\n"),
                ("build.rs", "fn main() {}\n"),
            ]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("src/bin/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert_eq!(not, ["check.yaml", "build.rs"], "Cargo.toml is unchanged");
        assert_eq!(
            std::fs::read_to_string(d.join("check.yaml")).unwrap(),
            "timeout_ms: 1\n"
        );
        assert!(!d.join("build.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_exercise_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("src")).unwrap();
        let err = write_files(dir.path(), &request(&[("src/main.rs", "x")])).unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(!outside.path().join("main.rs").exists());
    }

    #[test]
    fn a_page_that_names_no_mode_gets_the_run_the_exercise_has() {
        let exercise = |modes: &str, circuit: bool| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("ter.toml"),
                format!(
                    "exercise = \"e\"\ncourse = \"c\"\ntarget = \"t\"\nmodes = [{modes}]\n\
                     mode = \"hardware\"\nfetched_sha256 = \"\"\n"
                ),
            )
            .unwrap();
            if circuit {
                std::fs::write(dir.path().join("diagram.json"), "{}").unwrap();
            }
            dir
        };
        // The editor's Run button: a simulation wherever there is one,
        // whichever mode the exercise lists first.
        let both = exercise("\"hardware\", \"simulation\"", true);
        assert_eq!(mode_for(both.path(), None), "simulation");
        let sim = exercise("\"simulation\"", true);
        assert_eq!(mode_for(sim.path(), None), "simulation");
        // The board is all the exercise allows, or all it can run on.
        let hw = exercise("\"hardware\"", false);
        assert_eq!(mode_for(hw.path(), None), "hardware");
        let no_circuit = exercise("\"hardware\", \"simulation\"", false);
        assert_eq!(mode_for(no_circuit.path(), None), "hardware");
        // What the page names is what runs.
        assert_eq!(mode_for(hw.path(), Some("simulation")), "simulation");
        assert_eq!(mode_for(both.path(), Some("hardware")), "hardware");
        // Not an exercise folder: the run itself says so.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(mode_for(bare.path(), None), "simulation");
    }

    #[test]
    fn a_page_can_name_every_simulator_ter_run_can() {
        assert_eq!(ter_remote::serve::SIMULATORS, run::SIM_VENUES);
    }

    #[test]
    fn a_simulator_is_ready_or_says_why_not() {
        assert_eq!(
            simulator("wokwi", Ok(())),
            json!({"name": "wokwi", "ready": true})
        );
        assert_eq!(
            simulator(
                "sim86",
                Err(CliError::new(
                    "venue_unavailable",
                    "sim86 is not installed."
                ))
            ),
            json!({"name": "sim86", "ready": false, "reason": "sim86 is not installed."})
        );
    }

    #[test]
    fn the_limit_is_the_build_plus_the_venues_budget() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            limit(dir.path(), "simulation", None),
            BUILD_ALLOWANCE + run::wokwi_budget(0)
        );
        std::fs::write(
            dir.path().join("check.yaml"),
            "timeout_ms: 7000\nassert:\n  - id: b\n    serial_contains: hi\n    within_ms: 10\n",
        )
        .unwrap();
        assert_eq!(
            limit(dir.path(), "simulation", None),
            BUILD_ALLOWANCE + Duration::from_secs(120 + 140)
        );
        assert_eq!(
            limit(dir.path(), "simulation", Some("wokwi")),
            limit(dir.path(), "simulation", None)
        );
        assert_eq!(
            limit(dir.path(), "simulation", Some("sim86")),
            BUILD_ALLOWANCE + Sim86::budget(7000),
            "the simulator the page names, not the folder's default"
        );
        assert_eq!(
            limit(dir.path(), "hardware", None),
            BUILD_ALLOWANCE + FLASH_ALLOWANCE + Duration::from_secs(12)
        );
        assert_eq!(
            limit(dir.path(), "hardware", Some("local")),
            limit(dir.path(), "hardware", None)
        );
        assert_eq!(
            limit(dir.path(), "hardware", Some("bench")),
            limit(dir.path(), "hardware", None) + bench_queue() + BENCH_ALLOWANCE,
            "a bench run gets longer"
        );
    }
}
