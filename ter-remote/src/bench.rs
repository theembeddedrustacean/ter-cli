//! A shared bench: the owner's board, driven by someone else's `ter run`.
//!
//! The driver builds on their own machine, sends the program here, and
//! gets the recording back; they judge it and post the run as themselves.
//! The owner's machine never builds anyone's code and never posts for
//! them. The site is not on this path: it only told the driver where the
//! bench is and gave them the share code.
//!
//! The bench answers on its own socket, separate from the lesson page's
//! loopback service, because a tunnel or a LAN reaches it under names
//! the page's host guard would refuse. What guards it instead is the share
//! code, in the `X-TER-Share-Code` header of every request: the code the
//! site issued while sharing is on. Browsers are refused outright.
//!
//! Owner policy is [`crate::policy`]: one driver at a time, a session cap,
//! and the kill switch, which drops the driver even mid-run (their run's
//! result is not handed back).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use ter_telemetry::{Capture, EventKinds, Recording, Stimulus};
use ter_telemetry::{Recovery, Venue, VenueError, VenueFailure};

use crate::policy::{Claim, DriverLock, Limits};

pub const CODE_HEADER: &str = "x-ter-share-code";
pub const DRIVER_HEADER: &str = "x-ter-driver";
/// A program for a small board, base64 encoded, with room to spare.
pub const BODY_LIMIT: usize = 64 * 1024 * 1024;
/// The longest capture a driver may ask for.
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// A flash, on top of the capture.
pub const FLASH_ALLOWANCE: Duration = Duration::from_secs(180);

/// `GET /bench`: what the bench is and whether it is free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchInfo {
    pub service: String,
    pub version: String,
    /// The target id of the board (`xiao-esp32c3`).
    pub board: String,
    pub label: String,
    /// What a run here can see and drive.
    pub provides: EventKinds,
    /// Who holds the bench now, if anyone.
    pub driver: Option<String>,
}

/// How the board is flashed, as the driver's exercise says: never a
/// command line, only a known tool and a chip name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// `espflash` or `probe-rs`.
    pub program: String,
    pub chip: Option<String>,
}

impl ToolSpec {
    /// Why the bench will not flash with this, if it will not.
    pub fn check(&self) -> Result<(), String> {
        if !matches!(self.program.as_str(), "espflash" | "probe-rs") {
            return Err(format!(
                "a bench flashes with espflash or probe-rs, not {:?}",
                self.program
            ));
        }
        if self.program == "probe-rs" && self.chip.is_none() {
            return Err("probe-rs needs the chip".into());
        }
        if let Some(chip) = &self.chip
            && (chip.is_empty()
                || chip.len() > 40
                || !chip
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(format!("{chip:?} is not a chip name"));
        }
        Ok(())
    }
}

/// `POST /bench/run`: one program to flash and capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchRun {
    /// The exercise's target; the bench refuses one for another board.
    pub target: String,
    pub tool: ToolSpec,
    pub timeout_ms: u64,
    #[serde(default)]
    pub stimuli: Vec<Stimulus>,
    /// The capture header as the driver fills it (target, versions, the
    /// program's hash); the bench adds where and what it saw.
    pub capture: Capture,
    /// The ELF, base64.
    pub elf: String,
}

/// The recording of a run on the bench.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchAnswer {
    /// `events.jsonl`.
    pub events: String,
    #[serde(default)]
    pub flash_log: String,
    /// What the board sent, as text.
    #[serde(default)]
    pub serial_log: String,
}

/// A bench refusal or failure, as `{"error": {code, message, output}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchError {
    pub code: String,
    pub message: String,
    /// What the flasher or the board printed, for the run's transcript.
    #[serde(default)]
    pub output: String,
}

impl BenchError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            output: String::new(),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self.code.as_str() {
            "bad_request" | "wrong_board" => StatusCode::BAD_REQUEST,
            "not_sharing" | "share_code_refused" | "origin_refused" | "dropped" => {
                StatusCode::FORBIDDEN
            }
            "not_found" => StatusCode::NOT_FOUND,
            "bench_busy" => StatusCode::CONFLICT,
            "flash_failed" => StatusCode::BAD_GATEWAY,
            "venue_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The venue failure a driver posts this as: never the learner's
    /// fault, so never `failed`.
    pub fn failure(&self) -> VenueFailure {
        match self.code.as_str() {
            "bench_offline" => VenueFailure::BenchOffline,
            "flash_failed" => VenueFailure::FlashFailed,
            _ => VenueFailure::Unavailable,
        }
    }
}

impl IntoResponse for BenchError {
    fn into_response(self) -> Response {
        let status = self.status();
        (status, axum::Json(json!({ "error": self }))).into_response()
    }
}

pub type BoardFuture = Pin<Box<dyn Future<Output = Result<BenchAnswer, BenchError>> + Send>>;

/// The owner's board; ter-cli's implementation flashes and captures it.
pub trait Board: Send + Sync + 'static {
    fn board(&self) -> &str;
    fn label(&self) -> &str;
    fn provides(&self) -> EventKinds;
    /// Flash `elf` and capture the run. The request is already checked.
    fn run(&self, run: BenchRun, elf: Vec<u8>) -> BoardFuture;
}

/// Whether an exercise for `target` runs on `board`: the target is the
/// board, or the board with a variant after it (`xiao-esp32c3-nostd`).
pub fn fits(board: &str, target: &str) -> bool {
    target == board
        || target
            .strip_prefix(board)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// The owner's side of sharing: the code in force and who drives.
pub struct Owner {
    /// `None` while not sharing: every request is refused.
    code: Option<String>,
    /// The code the kill switch voided: a stale read of the site that
    /// still shows it must not bring it back.
    voided: Option<String>,
    lock: DriverLock,
}

pub type SharedOwner = Arc<Mutex<Owner>>;

impl Owner {
    pub fn new(limits: Limits) -> SharedOwner {
        Arc::new(Mutex::new(Self {
            code: None,
            voided: None,
            lock: DriverLock::new(limits),
        }))
    }

    /// The share code the site issued, or `None` when sharing is off. A
    /// new code, or none, drops whoever was driving.
    pub fn set_code(&mut self, code: Option<String>) -> Option<String> {
        if self.code == code || (code.is_some() && code == self.voided) {
            return None;
        }
        self.code = code;
        self.lock.kill()
    }

    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// The kill switch: sharing off here and the driver dropped. The
    /// caller also turns sharing off on the site, which voids the code.
    pub fn kill(&mut self) -> Option<String> {
        self.voided = self.code.take().or(self.voided.take());
        self.lock.kill()
    }

    /// A run is in flight: heartbeats say `busy`.
    pub fn busy(&self) -> bool {
        self.lock.running()
    }

    pub fn driver(&mut self) -> Option<String> {
        self.lock.driver(Instant::now()).map(str::to_string)
    }
}

#[derive(Clone)]
struct Ctx {
    board: Arc<dyn Board>,
    owner: SharedOwner,
}

/// The bench's routes, behind the share-code guard.
pub fn router(board: Arc<dyn Board>, owner: SharedOwner) -> Router {
    let ctx = Ctx { board, owner };
    Router::new()
        .route("/bench", get(info))
        .route("/bench/run", post(run))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(ctx)
}

async fn not_found() -> BenchError {
    BenchError::new("not_found", "A bench answers /bench and /bench/run.")
}

/// Compared without stopping at the first difference, so timing does not
/// give the code away a character at a time.
fn same_code(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn header_text(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

async fn guard(State(ctx): State<Ctx>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let driver = header_text(&request, DRIVER_HEADER).unwrap_or_else(|| "someone".into());
    let offered = header_text(&request, CODE_HEADER).unwrap_or_default();
    let from_page = request.headers().contains_key(header::ORIGIN);
    let refused = if from_page {
        Some(BenchError::new(
            "origin_refused",
            "A bench takes runs from ter, not from a web page.",
        ))
    } else {
        let code = ctx.owner.lock().expect("owner lock").code.clone();
        match code {
            None => Some(BenchError::new(
                "not_sharing",
                "The owner is not sharing this bench right now.",
            )),
            Some(code) if !same_code(&code, &offered) => Some(BenchError::new(
                "share_code_refused",
                "That is not this bench's share code. Ask the owner for the current one and run `ter connect <code>`.",
            )),
            Some(_) => None,
        }
    };
    let response = match refused {
        Some(e) => e.into_response(),
        None => next.run(request).await,
    };
    eprintln!(
        "bench: {method} {path} from {driver} -> {}",
        response.status().as_u16()
    );
    response
}

async fn info(State(ctx): State<Ctx>) -> Response {
    let driver = ctx.owner.lock().expect("owner lock").driver();
    axum::Json(BenchInfo {
        service: "ter-bench".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        board: ctx.board.board().into(),
        label: ctx.board.label().into(),
        provides: ctx.board.provides(),
        driver,
    })
    .into_response()
}

async fn run(State(ctx): State<Ctx>, request: Request) -> Response {
    let driver = header_text(&request, DRIVER_HEADER).filter(|d| !d.is_empty());
    let Some(driver) = driver else {
        return BenchError::new("bad_request", "Say who is driving in X-TER-Driver.")
            .into_response();
    };
    let body = match axum::body::to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            return BenchError::new("bad_request", format!("Could not read the run: {e}"))
                .into_response();
        }
    };
    match run_checked(&ctx, &driver, body).await {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn run_checked(ctx: &Ctx, driver: &str, body: Bytes) -> Result<BenchAnswer, BenchError> {
    let run: BenchRun = serde_json::from_slice(&body)
        .map_err(|e| BenchError::new("bad_request", format!("The run is not valid: {e}")))?;
    run.tool
        .check()
        .map_err(|why| BenchError::new("bad_request", format!("Refused: {why}.")))?;
    if !fits(ctx.board.board(), &run.target) {
        return Err(BenchError::new(
            "wrong_board",
            format!(
                "This bench is a {}, and the exercise is for {}.",
                ctx.board.board(),
                run.target
            ),
        ));
    }
    if run.timeout_ms == 0 || run.timeout_ms > MAX_TIMEOUT_MS {
        return Err(BenchError::new(
            "bad_request",
            format!(
                "A capture on a bench is 1 to {MAX_TIMEOUT_MS} ms, not {}.",
                run.timeout_ms
            ),
        ));
    }
    let elf = base64::engine::general_purpose::STANDARD
        .decode(run.elf.as_bytes())
        .map_err(|e| BenchError::new("bad_request", format!("The program is not base64: {e}")))?;

    let claim = ctx
        .owner
        .lock()
        .expect("owner lock")
        .lock
        .claim(driver, Instant::now());
    match claim {
        Claim::Busy {
            driver: holder,
            free_in,
        } => {
            return Err(BenchError::new(
                "bench_busy",
                format!(
                    "{holder} is using this bench; it is free in {} s at the latest.",
                    free_in.as_secs().max(1)
                ),
            ));
        }
        Claim::Granted { new_session: true } => {
            eprintln!("bench: {driver} is driving now");
        }
        Claim::Granted { new_session: false } => {}
    }
    let answer = ctx.board.run(run, elf).await;
    let mut owner = ctx.owner.lock().expect("owner lock");
    // Dropped by the kill switch while the board ran: the result is not
    // theirs to have.
    if owner.lock.driver(Instant::now()) != Some(driver) {
        return Err(BenchError::new(
            "dropped",
            "The owner dropped you from this bench during the run.",
        ));
    }
    owner.lock.finished(driver, Instant::now());
    answer
}

/// Serve the bench on `listener` until the process ends.
pub async fn serve(
    listener: tokio::net::TcpListener,
    board: Arc<dyn Board>,
    owner: SharedOwner,
) -> std::io::Result<()> {
    axum::serve(listener, router(board, owner)).await
}

/// The driver's side: one connected bench.
#[derive(Clone)]
pub struct BenchClient {
    http: reqwest::Client,
    url: String,
    code: String,
    driver: String,
}

impl BenchClient {
    pub fn new(url: &str, code: &str, driver: &str) -> Result<Self, BenchError> {
        let parsed = reqwest::Url::parse(url).map_err(|e| {
            BenchError::new(
                "venue_unavailable",
                format!("The bench's address {url:?} is not a URL: {e}"),
            )
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(BenchError::new(
                "venue_unavailable",
                format!("The bench's address {url:?} is not http or https."),
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent(format!("ter-cli/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| BenchError::new("venue_unavailable", e.to_string()))?;
        Ok(Self {
            http,
            url: url.trim_end_matches('/').to_string(),
            code: code.to_string(),
            driver: driver.to_string(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .header(CODE_HEADER, &self.code)
            .header(DRIVER_HEADER, &self.driver)
    }

    /// What the bench is and who drives it; `bench_offline` when it does
    /// not answer.
    pub async fn info(&self) -> Result<BenchInfo, BenchError> {
        let req = self
            .request(reqwest::Method::GET, "/bench")
            .timeout(Duration::from_secs(20));
        self.send(req).await
    }

    /// Flash and capture on the bench.
    pub async fn run(&self, run: &BenchRun) -> Result<BenchAnswer, BenchError> {
        let limit =
            FLASH_ALLOWANCE + Duration::from_millis(run.timeout_ms) + Duration::from_secs(60);
        let req = self
            .request(reqwest::Method::POST, "/bench/run")
            .json(run)
            .timeout(limit);
        self.send(req).await
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, BenchError> {
        let offline = |e: reqwest::Error| {
            let mut msg = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                msg = format!("{msg}: {s}");
                source = s.source();
            }
            BenchError::new(
                "bench_offline",
                format!("The bench at {} did not answer: {msg}", self.url),
            )
        };
        let resp = req.send().await.map_err(offline)?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(offline)?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|e| {
                BenchError::new(
                    "venue_unavailable",
                    format!("The bench's answer was not what ter expects: {e}"),
                )
            });
        }
        #[derive(Deserialize)]
        struct Envelope {
            error: BenchError,
        }
        match serde_json::from_slice::<Envelope>(&body) {
            Ok(env) => Err(env.error),
            // A tunnel's own error page: the bench behind it is not there.
            Err(_) if matches!(status.as_u16(), 502..=504) => Err(BenchError::new(
                "bench_offline",
                format!(
                    "The bench at {} is not answering (HTTP {status}).",
                    self.url
                ),
            )),
            Err(_) => Err(BenchError::new(
                "venue_unavailable",
                format!("The bench at {} answered HTTP {status}.", self.url),
            )),
        }
    }
}

/// A connected bench as the venue of one `ter run`.
pub struct RemoteBench {
    pub client: BenchClient,
    pub target: String,
    pub tool: ToolSpec,
    pub timeout_ms: u64,
    /// What `GET /bench` said the bench sees.
    pub provides: EventKinds,
    pub capture: Capture,
    pub run_dir: PathBuf,
    elf: Option<Vec<u8>>,
}

impl RemoteBench {
    pub fn new(
        client: BenchClient,
        info: &BenchInfo,
        target: &str,
        tool: ToolSpec,
        timeout_ms: u64,
        run_dir: PathBuf,
    ) -> Self {
        Self {
            client,
            target: target.into(),
            tool,
            timeout_ms,
            provides: info.provides.clone(),
            capture: Capture::default(),
            run_dir,
            elf: None,
        }
    }
}

/// What the flasher printed, in the run folder: the same name the local
/// venue uses.
pub const FLASH_LOG: &str = "flash.log";
pub const SERIAL_LOG: &str = "serial.log";

impl Venue for RemoteBench {
    fn name(&self) -> &'static str {
        "bench"
    }

    fn provides(&self) -> EventKinds {
        self.provides.clone()
    }

    fn prepare(&mut self, elf: &Path) -> Result<(), VenueError> {
        let bytes = std::fs::read(elf).map_err(|e| {
            VenueError::unavailable(format!("Could not read {}: {e}", elf.display()), "")
        })?;
        self.elf = Some(bytes);
        Ok(())
    }

    /// The bench resets the board after flashing it.
    fn reset(&mut self) -> Result<(), VenueError> {
        Ok(())
    }

    fn run(&mut self, stimuli: &[Stimulus], _budget: Duration) -> Result<Recording, VenueError> {
        let elf = self
            .elf
            .take()
            .ok_or_else(|| VenueError::unavailable("No program to send to the bench", ""))?;
        let run = BenchRun {
            target: self.target.clone(),
            tool: self.tool.clone(),
            timeout_ms: self.timeout_ms,
            stimuli: stimuli.to_vec(),
            capture: self.capture.clone(),
            elf: base64::engine::general_purpose::STANDARD.encode(elf),
        };
        let client = self.client.clone();
        // The venue contract is blocking; ter runs it on its runtime.
        let answer = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(client.run(&run))
        });
        let answer = answer.map_err(|e| {
            let _ = std::fs::write(self.run_dir.join(FLASH_LOG), &e.output);
            VenueError {
                failure: e.failure(),
                message: e.message,
                output: e.output,
            }
        })?;
        let _ = std::fs::write(self.run_dir.join(FLASH_LOG), &answer.flash_log);
        let _ = std::fs::write(self.run_dir.join(SERIAL_LOG), &answer.serial_log);
        let mut rec = Recording::from_jsonl(&answer.events).map_err(|e| {
            VenueError::unavailable(
                format!("The bench sent a recording ter cannot read: {e}"),
                "",
            )
        })?;
        rec.capture.venue = "bench".into();
        Ok(rec)
    }

    fn recover(&mut self) -> Recovery {
        Recovery::NotNeeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use ter_telemetry::{Event, Kind};
    use tower::ServiceExt;

    struct FakeBoard {
        runs: Mutex<Vec<BenchRun>>,
        /// Held by the test to keep a run in flight.
        gate: Arc<tokio::sync::Mutex<()>>,
    }

    impl Board for FakeBoard {
        fn board(&self) -> &str {
            "xiao-esp32c3"
        }
        fn label(&self) -> &str {
            "Desk"
        }
        fn provides(&self) -> EventKinds {
            [Kind::Serial, Kind::Reset].into()
        }
        fn run(&self, run: BenchRun, elf: Vec<u8>) -> BoardFuture {
            assert_eq!(elf, b"\x7fELF program");
            self.runs.lock().unwrap().push(run.clone());
            let gate = self.gate.clone();
            Box::pin(async move {
                let _held = gate.lock().await;
                let rec = Recording {
                    capture: Capture {
                        venue: "local".into(),
                        ..run.capture
                    },
                    events: vec![Event::reset(0), Event::serial(10, "hello\n")],
                };
                Ok(BenchAnswer {
                    events: rec.to_jsonl(),
                    flash_log: "Flashing has completed!".into(),
                    serial_log: "hello\n".into(),
                })
            })
        }
    }

    fn bench(code: Option<&str>) -> (Router, SharedOwner, Arc<FakeBoard>) {
        let board = Arc::new(FakeBoard {
            runs: Mutex::new(Vec::new()),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        });
        let owner = Owner::new(Limits::default());
        owner.lock().unwrap().set_code(code.map(str::to_string));
        (router(board.clone(), owner.clone()), owner, board)
    }

    fn run_body() -> String {
        serde_json::to_string(&BenchRun {
            target: "xiao-esp32c3-nostd".into(),
            tool: ToolSpec {
                program: "espflash".into(),
                chip: Some("esp32c3".into()),
            },
            timeout_ms: 1500,
            stimuli: vec![],
            capture: Capture {
                target: "xiao-esp32c3-nostd".into(),
                cli_version: "0.1.0".into(),
                ..Capture::default()
            },
            elf: base64::engine::general_purpose::STANDARD.encode(b"\x7fELF program"),
        })
        .unwrap()
    }

    fn req(method: &str, path: &str, code: Option<&str>, driver: &str, body: String) -> Request {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "bench.example.net")
            .header(DRIVER_HEADER, driver)
            .header("content-type", "application/json");
        if let Some(c) = code {
            b = b.header(CODE_HEADER, c);
        }
        b.body(Body::from(body)).unwrap()
    }

    async fn send(app: &Router, r: Request) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn only_the_current_share_code_gets_in() {
        let (app, owner, board) = bench(Some("GZTS-2520"));
        let (status, v) = send(&app, req("GET", "/bench", None, "ana", String::new())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "share_code_refused");
        let (status, v) = send(
            &app,
            req("POST", "/bench/run", Some("GZTS-2521"), "ana", run_body()),
        )
        .await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("share_code_refused"))
        );
        assert!(board.runs.lock().unwrap().is_empty(), "nothing flashed");

        let (status, v) = send(
            &app,
            req("GET", "/bench", Some("GZTS-2520"), "ana", String::new()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["service"], "ter-bench");
        assert_eq!(v["board"], "xiao-esp32c3");
        assert_eq!(v["provides"], json!(["serial", "reset"]));
        assert_eq!(v["driver"], json!(null));

        owner.lock().unwrap().set_code(None);
        let (status, v) = send(
            &app,
            req("GET", "/bench", Some("GZTS-2520"), "ana", String::new()),
        )
        .await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("not_sharing"))
        );
    }

    #[tokio::test]
    async fn a_web_page_is_refused_even_with_the_code() {
        let (app, _, _) = bench(Some("GZTS-2520"));
        let mut r = req("GET", "/bench", Some("GZTS-2520"), "ana", String::new());
        r.headers_mut()
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let (status, v) = send(&app, r).await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("origin_refused"))
        );
    }

    #[tokio::test]
    async fn a_run_comes_back_as_a_recording() {
        let (app, owner, board) = bench(Some("GZTS-2520"));
        let (status, v) = send(
            &app,
            req("POST", "/bench/run", Some("GZTS-2520"), "ana", run_body()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let rec = Recording::from_jsonl(v["events"].as_str().unwrap()).unwrap();
        assert_eq!(rec.serial_text(), "hello\n");
        assert_eq!(
            rec.capture.cli_version, "0.1.0",
            "the driver's header, kept"
        );
        assert_eq!(
            board.runs.lock().unwrap()[0].tool.chip.as_deref(),
            Some("esp32c3")
        );
        assert_eq!(owner.lock().unwrap().driver().as_deref(), Some("ana"));
        assert!(!owner.lock().unwrap().busy());
    }

    #[tokio::test]
    async fn a_second_driver_waits_and_the_kill_switch_drops_the_first() {
        let (app, owner, board) = bench(Some("GZTS-2520"));
        let held = board.gate.clone().lock_owned().await;
        let first = tokio::spawn({
            let app = app.clone();
            async move {
                send(
                    &app,
                    req("POST", "/bench/run", Some("GZTS-2520"), "ana", run_body()),
                )
                .await
            }
        });
        while !owner.lock().unwrap().busy() {
            tokio::task::yield_now().await;
        }
        let (status, v) = send(
            &app,
            req("POST", "/bench/run", Some("GZTS-2520"), "ben", run_body()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"]["code"], "bench_busy");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("ana is using")
        );

        assert_eq!(owner.lock().unwrap().kill().as_deref(), Some("ana"));
        drop(held);
        let (status, v) = first.await.unwrap();
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(v["error"]["code"], "dropped");
        let (_, v) = send(
            &app,
            req("POST", "/bench/run", Some("GZTS-2520"), "ben", run_body()),
        )
        .await;
        assert_eq!(
            v["error"]["code"], "not_sharing",
            "the kill switch also stops sharing"
        );
    }

    #[tokio::test]
    async fn a_run_for_another_board_or_tool_is_refused_before_flashing() {
        let (app, _, board) = bench(Some("GZTS-2520"));
        let mut other: BenchRun = serde_json::from_str(&run_body()).unwrap();
        other.target = "xiao-rp2040".into();
        let (status, v) = send(
            &app,
            req(
                "POST",
                "/bench/run",
                Some("GZTS-2520"),
                "ana",
                serde_json::to_string(&other).unwrap(),
            ),
        )
        .await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::BAD_REQUEST, &json!("wrong_board"))
        );
        let mut tool: BenchRun = serde_json::from_str(&run_body()).unwrap();
        tool.tool.program = "sh".into();
        let (_, v) = send(
            &app,
            req(
                "POST",
                "/bench/run",
                Some("GZTS-2520"),
                "ana",
                serde_json::to_string(&tool).unwrap(),
            ),
        )
        .await;
        assert_eq!(v["error"]["code"], "bad_request");
        tool.tool = ToolSpec {
            program: "espflash".into(),
            chip: Some("esp32c3 --port /dev/x".into()),
        };
        let (_, v) = send(
            &app,
            req(
                "POST",
                "/bench/run",
                Some("GZTS-2520"),
                "ana",
                serde_json::to_string(&tool).unwrap(),
            ),
        )
        .await;
        assert_eq!(v["error"]["code"], "bad_request");
        let (_, v) = send(
            &app,
            req("POST", "/bench/run", Some("GZTS-2520"), "", run_body()),
        )
        .await;
        assert_eq!(v["error"]["code"], "bad_request", "no driver named");
        assert!(board.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn exercises_fit_their_board() {
        assert!(fits("xiao-esp32c3", "xiao-esp32c3"));
        assert!(fits("xiao-esp32c3", "xiao-esp32c3-nostd"));
        assert!(!fits("xiao-esp32c3", "xiao-esp32c6-nostd"));
        assert!(!fits("xiao-esp32c", "xiao-esp32c3"));
    }

    #[test]
    fn a_new_code_or_none_drops_the_driver() {
        let owner = Owner::new(Limits::default());
        let mut o = owner.lock().unwrap();
        o.set_code(Some("AAAA-0001".into()));
        o.lock.claim("ana", Instant::now());
        assert_eq!(
            o.set_code(Some("AAAA-0001".into())),
            None,
            "same code, nobody dropped"
        );
        assert_eq!(o.set_code(Some("BBBB-0002".into())).as_deref(), Some("ana"));
        assert_eq!(o.kill(), None);
        assert_eq!(o.code(), None);
        o.set_code(Some("BBBB-0002".into()));
        assert_eq!(o.code(), None, "a stale read cannot revive the voided code");
        o.set_code(Some("CCCC-0003".into()));
        assert_eq!(
            o.code(),
            Some("CCCC-0003"),
            "a new code from `ter bench share` works"
        );
        assert!(same_code("AAAA-0001", "AAAA-0001"));
        assert!(!same_code("AAAA-0001", "AAAA-0002"));
        assert!(!same_code("AAAA-0001", "AAAA-000"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_driver_gets_offline_and_refusals_as_venue_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (app, _, _) = bench(Some("GZTS-2520"));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let good = BenchClient::new(&url, "GZTS-2520", "ana").unwrap();
        let info = good.info().await.unwrap();
        assert_eq!(info.board, "xiao-esp32c3");

        let wrong = BenchClient::new(&url, "ZZZZ-0000", "ana").unwrap();
        let e = wrong.info().await.unwrap_err();
        assert_eq!(e.code, "share_code_refused");
        assert_eq!(e.failure(), VenueFailure::Unavailable);

        let gone = BenchClient::new("http://127.0.0.1:9", "GZTS-2520", "ana").unwrap();
        let e = gone.info().await.unwrap_err();
        assert_eq!(e.code, "bench_offline");
        assert_eq!(e.failure(), VenueFailure::BenchOffline);

        let dir = tempfile::tempdir().unwrap();
        let elf = dir.path().join("firmware.elf");
        std::fs::write(&elf, b"\x7fELF program").unwrap();
        let tool = ToolSpec {
            program: "espflash".into(),
            chip: Some("esp32c3".into()),
        };
        let mut venue = RemoteBench::new(
            good,
            &info,
            "xiao-esp32c3-nostd",
            tool,
            1500,
            dir.path().into(),
        );
        venue.prepare(&elf).unwrap();
        venue.reset().unwrap();
        let rec = venue.run(&[], Duration::from_secs(10)).unwrap();
        assert_eq!(rec.capture.venue, "bench");
        assert_eq!(rec.serial_text(), "hello\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FLASH_LOG)).unwrap(),
            "Flashing has completed!"
        );
    }
}
