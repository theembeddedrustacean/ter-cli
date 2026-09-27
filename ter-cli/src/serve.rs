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
//! One run at a time: a second request waits for the first, since both
//! would build in the same course cache.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use ter_check::CheckFile;
use ter_remote::serve::{RunFuture, RunRequest, ServeError, Service, editable};

use crate::config::Config;
use crate::exercises;
use crate::output::CliError;
use crate::run::{self, CHECK_YAML, RunArgs};
use crate::session::Session;
use crate::share;

/// How long a build may take: the first build of a course compiles the HAL
/// (and, for ESP-IDF, the IDF) from nothing.
pub const BUILD_ALLOWANCE: Duration = Duration::from_secs(600);
/// A board's flash, on top of its capture.
const FLASH_ALLOWANCE: Duration = Duration::from_secs(180);

struct Inner {
    /// Held for the whole of a run, past a timeout answer too.
    turn: Arc<tokio::sync::Mutex<()>>,
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
        json!({"version": env!("CARGO_PKG_VERSION"), "posts_to_site": posts})
    }

    fn run(&self, request: RunRequest) -> RunFuture {
        let inner = self.0.clone();
        Box::pin(async move { handle(inner, request).await.map_err(|f| f.0) })
    }
}

async fn handle(inner: Arc<Inner>, request: RunRequest) -> Result<Value, ServeFailure> {
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
    let limit = limit(&dir, &request.mode);
    let mode: &'static str = if request.mode == "hardware" {
        "hardware"
    } else {
        "simulation"
    };
    eprintln!(
        "Running {} ({mode}) in {}",
        request.exercise_id,
        dir.display()
    );

    // The run goes on in its own task, holding the turn, so a timeout
    // answers the page without leaving a second build to start under it.
    let task = tokio::spawn(async move {
        let done = run::execute(
            RunArgs {
                dir: Some(dir),
                mode: Some(mode),
                venue: None,
                no_check: false,
            },
            true,
        )
        .await;
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

/// The most a run may take: the build, then the venue's own budget for
/// the check's `timeout_ms`.
fn limit(dir: &Path, mode: &str) -> Duration {
    let timeout_ms = std::fs::read_to_string(dir.join(CHECK_YAML))
        .ok()
        .and_then(|t| CheckFile::parse(&t).ok())
        .map_or(0, |f| f.timeout_ms);
    let run = if mode == "hardware" {
        FLASH_ALLOWANCE + run::board_budget(timeout_ms)
    } else {
        run::wokwi_budget(timeout_ms)
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
    share: Option<ShareFlags>,
    json: bool,
) -> Result<(), CliError> {
    let port = match port {
        Some(p) => p,
        None => Config::load()?.serve_port,
    };
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
        format!("{}\n", json!({"url": url, "port": bound, "bench": bench}))
    } else {
        let mut b = format!(
            "ter serve on {url}, {who}\nThe lesson page's Run button builds here. Stop with Ctrl-C.\n"
        );
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

    let service = Arc::new(Serve(Arc::new(Inner { turn })));
    ter_remote::serve::serve(listener, service)
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
            mode: "simulation".into(),
            user: None,
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
    fn the_limit_is_the_build_plus_the_venues_budget() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            limit(dir.path(), "simulation"),
            BUILD_ALLOWANCE + run::wokwi_budget(0)
        );
        std::fs::write(
            dir.path().join("check.yaml"),
            "timeout_ms: 7000\nassert:\n  - id: b\n    serial_contains: hi\n    within_ms: 10\n",
        )
        .unwrap();
        assert_eq!(
            limit(dir.path(), "simulation"),
            BUILD_ALLOWANCE + Duration::from_secs(120 + 140)
        );
        assert_eq!(
            limit(dir.path(), "hardware"),
            BUILD_ALLOWANCE + FLASH_ALLOWANCE + Duration::from_secs(12)
        );
    }
}
