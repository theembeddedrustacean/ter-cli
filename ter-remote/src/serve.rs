//! `ter serve`: the localhost service the lesson page's editor calls.
//!
//! The page (on the TER Learn site, or the fork's dev server) posts the
//! learner's files to `http://127.0.0.1:<port>/run`; the service builds,
//! runs and checks them, posts the run with the machine's own token and
//! answers with the run it posted. This module is the HTTP side: routes,
//! CORS, the origin and host guards and body validation. What a run does
//! is the [`Service`] ter-cli plugs in.
//!
//! A run on a simulator that can show it as it happens may be asked for
//! live (`"live": {"limit_s": 60}` in `/run`): the answer then names a live
//! run, and the page follows it on `/live/<id>/events` (Server-Sent
//! Events), lengthens it with `/live/<id>/extend`, pauses and resumes it
//! with `/live/<id>/pause` and `/live/<id>/resume`, starts it again from
//! its time 0 with `/live/<id>/reset`, and ends it with `/live/<id>/stop`. `/health` says `"live": true` when the service does
//! this.
//!
//! Only the lesson page's origin gets an answer a browser will read (and
//! the fork's dev server, when `ter serve --dev` asks for it), and any
//! other origin is refused before anything runs. The lesson page's origin
//! is that of the site this ter is set to talk to, the one its runs are
//! posted to: a ter set to another copy of the site answers that copy's
//! page and not the live one's. Refusing comes first because a POST from an unknown
//! page must not build or post, whatever CORS lets it read. A `/run` with
//! no `Origin` at all is refused too: every browser sends one on a
//! cross-origin POST, so a request without it is not the lesson page. A
//! `Host` that is not this loopback address is refused as well, so a site
//! that rebinds its own name to 127.0.0.1 cannot reach the service.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};

/// The origin a browser sends for a page on the site at `site_url`: scheme,
/// host and port (left out when it is the scheme's own), with no path and
/// no trailing slash. `None` when the address is not an http or https one.
pub fn origin_of(site_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(site_url.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

/// The fork's dev server: trusted only when `ter serve --dev` says so, since
/// anything a learner runs on that port would otherwise count as the page.
pub const DEV_ORIGIN: &str = "http://localhost:8080";

/// The largest body `/run` takes: an exercise's files, with room to spare.
pub const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// One file from the page's editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageFile {
    pub path: String,
    pub content: String,
}

/// The simulators a page can name as the `venue` of a simulation.
pub const SIMULATORS: [&str; 2] = ["wokwi", "sim86"];

/// A validated `/run` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub exercise_id: String,
    pub files: Vec<PageFile>,
    /// `simulation` or `hardware`. `None` when the page does not say: the
    /// service then runs a simulation where the exercise has one (the
    /// editor is the simulation path), else on the board.
    pub mode: Option<String>,
    /// Where the run goes within its mode. Hardware: `local` (this
    /// machine's board) or `bench` (a shared bench this account connected
    /// to). Simulation: one of [`SIMULATORS`]. `None` is the exercise's own
    /// venue, as before the field existed.
    pub venue: Option<String>,
    /// The bench to run on (its `name` on the Devices list) when `venue`
    /// is `bench`; `None` is the bench this account connected to last.
    pub bench: Option<String>,
    /// The page's signed-in user, when the page sends it.
    pub user: Option<String>,
    /// Set when the page wants to watch the run as it happens: how many
    /// seconds it may last before it is extended.
    pub live: Option<u64>,
}

/// How long a live run lasts when the page does not say.
pub const LIVE_LIMIT_S: u64 = 60;
/// The longest a live run may last in total, extensions included.
pub const LIVE_CAP_S: u64 = 600;

/// One event of a live run, as `/live/<id>/events` sends it: `name` is
/// the SSE event's name (`pin`, `serial`, `checks`, `limit`, `paused`,
/// `resumed`, `reset`, `ended`) and `data` its one JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveEvent {
    pub name: &'static str,
    pub data: Value,
}

/// A live run's events from now on; it closes after `ended`.
pub type LiveEvents = tokio::sync::mpsc::Receiver<LiveEvent>;

/// What the lesson page asks of a running board, beside extending and
/// stopping it.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveControl {
    /// Hold the board at its simulated time.
    Pause,
    /// Let a paused board run on.
    Resume,
    /// Start the board again from its time 0, with the same program.
    Reset,
}

/// A failure `ter serve` answers with, as `{"error": {code, message}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeError {
    pub code: String,
    pub message: String,
}

impl ServeError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    pub fn status(&self) -> StatusCode {
        match self.code.as_str() {
            "bad_request" | "mode_not_allowed" | "not_observable" | "bad_check" | "wrong_board" => {
                StatusCode::BAD_REQUEST
            }
            "no_token" | "not_paired" | "token_invalid" | "token_revoked" | "token_expired" => {
                StatusCode::UNAUTHORIZED
            }
            "wrong_account" | "not_enrolled" | "locked" | "origin_refused" | "host_refused" => {
                StatusCode::FORBIDDEN
            }
            "not_found" | "unknown_bench" => StatusCode::NOT_FOUND,
            "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "timeout" => StatusCode::GATEWAY_TIMEOUT,
            "busy" | "bench_busy" | "limit_cap" | "ended" | "replaced" => StatusCode::CONFLICT,
            "outdated" => StatusCode::UPGRADE_REQUIRED,
            "venue_unavailable" | "bench_offline" | "no_capacity" => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ServeError {
    fn into_response(self) -> Response {
        (
            self.status(),
            axum::Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

pub type RunFuture = Pin<Box<dyn Future<Output = Result<Value, ServeError>> + Send>>;

/// What the service does; ter-cli's implementation builds and posts.
pub trait Service: Send + Sync + 'static {
    /// `/health`'s answer, less `service`.
    fn health(&self) -> Value;
    /// Run the request; `Ok` is the run as posted (its build or check may
    /// have failed), `Err` a run that was not posted.
    fn run(&self, request: RunRequest) -> RunFuture;
    /// The events of live run `id` from now on; `None` when there is no
    /// such run.
    fn live_events(&self, _id: &str) -> Option<LiveEvents> {
        None
    }
    /// Let live run `id` go on `add_s` seconds longer: `{"ends_at"}`.
    fn live_extend(&self, id: &str, _add_s: u64) -> Result<Value, ServeError> {
        Err(no_live(id))
    }
    /// Stop live run `id`: `{"ok": true}`.
    fn live_stop(&self, id: &str) -> Result<Value, ServeError> {
        Err(no_live(id))
    }
    /// Do `control` to live run `id`, once the simulator has done it:
    /// `{"ok": true, ...}`.
    fn live_control(&self, id: &str, _control: LiveControl) -> RunFuture {
        let no = no_live(id);
        Box::pin(async move { Err(no) })
    }
    /// The address of the simulator's own page for live run `id`, signed
    /// in as the learner, for `/live/<id>/view` to send the browser to.
    /// It may hold the learner's key: it goes into the `Location` of the
    /// answer and nowhere else. May take a moment (it is called off the
    /// async threads).
    fn live_view(&self, id: &str) -> Result<String, ServeError> {
        Err(no_live(id))
    }
}

/// The answer for a live run this service does not have.
pub fn no_live(id: &str) -> ServeError {
    ServeError::new(
        "not_found",
        format!("ter serve has no live run {id:?}. A new run replaces the one before."),
    )
}

impl RunRequest {
    /// The body, or why it is not a run request.
    pub fn parse(body: &[u8]) -> Result<Self, ServeError> {
        let v: Value = serde_json::from_slice(body)
            .map_err(|e| ServeError::bad_request(format!("The body is not JSON: {e}")))?;
        let obj = v
            .as_object()
            .ok_or_else(|| ServeError::bad_request("The body must be a JSON object."))?;
        let exercise_id = obj
            .get("exercise_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ServeError::bad_request("exercise_id is required."))?
            .to_string();
        let files = match obj.get("files") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let field = |k: &str| {
                        f.get(k).and_then(Value::as_str).ok_or_else(|| {
                            ServeError::bad_request(format!("files[{i}].{k} must be a string."))
                        })
                    };
                    let path = field("path")?.to_string();
                    check_path(&path).map_err(|why| {
                        ServeError::bad_request(format!("files[{i}].path {path:?} {why}."))
                    })?;
                    Ok(PageFile {
                        path,
                        content: field("content")?.to_string(),
                    })
                })
                .collect::<Result<_, ServeError>>()?,
            Some(_) => return Err(ServeError::bad_request("files must be a list.")),
        };
        let text = |k: &str| match obj.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(_) => Err(ServeError::bad_request(format!("{k} must be a string."))),
        };
        let bench = text("bench")?;
        let venue = match text("venue")? {
            None if bench.is_some() => Some("bench".to_string()),
            None => None,
            Some(v) if v == "local" || v == "bench" || SIMULATORS.contains(&v.as_str()) => Some(v),
            Some(other) => {
                return Err(ServeError::bad_request(format!(
                    "venue must be \"local\" or \"bench\", or a simulator ({}), not {other:?}.",
                    SIMULATORS.join(", ")
                )));
            }
        };
        let simulator = venue.as_deref().is_some_and(|v| SIMULATORS.contains(&v));
        if let (Some(_), Some(v)) = (&bench, &venue)
            && v != "bench"
        {
            return Err(ServeError::bad_request(format!(
                "bench names a shared bench; it goes with venue \"bench\", not {v:?}."
            )));
        }
        let mode = match (obj.get("mode"), &venue) {
            // A simulator runs a simulation; any other venue is a place to
            // run hardware.
            (None | Some(Value::Null), Some(_)) if simulator => Some("simulation".to_string()),
            (None | Some(Value::Null), Some(_)) => Some("hardware".to_string()),
            (None | Some(Value::Null), None) => None,
            (Some(Value::String(m)), Some(v)) if m == "simulation" && !simulator => {
                return Err(ServeError::bad_request(format!(
                    "venue {v:?} goes with mode \"hardware\"; a simulation names a simulator ({}) or no venue.",
                    SIMULATORS.join(", ")
                )));
            }
            (Some(Value::String(m)), Some(v)) if m == "hardware" && simulator => {
                return Err(ServeError::bad_request(format!(
                    "venue {v:?} is a simulator; it goes with mode \"simulation\"."
                )));
            }
            (Some(Value::String(m)), _) if m == "simulation" || m == "hardware" => Some(m.clone()),
            (Some(other), _) => {
                return Err(ServeError::bad_request(format!(
                    "mode must be \"simulation\" or \"hardware\", not {other}."
                )));
            }
        };
        let user = text("user")?;
        let live = match obj.get("live") {
            None | Some(Value::Null) => None,
            Some(Value::Object(live)) => match live.get("limit_s") {
                None | Some(Value::Null) => Some(LIVE_LIMIT_S),
                Some(v) => match v.as_u64() {
                    Some(s) if (1..=LIVE_CAP_S).contains(&s) => Some(s),
                    _ => {
                        return Err(ServeError::bad_request(format!(
                            "live.limit_s must be a whole number of seconds from 1 to {LIVE_CAP_S}."
                        )));
                    }
                },
            },
            Some(_) => {
                return Err(ServeError::bad_request(
                    "live must be an object, {\"limit_s\": 60}.",
                ));
            }
        };
        if live.is_some() && (mode.as_deref() == Some("hardware") || bench.is_some()) {
            return Err(ServeError::bad_request(
                "A live run is a simulation on sim86 for now; hardware runs answer as before.",
            ));
        }
        Ok(Self {
            exercise_id,
            files,
            mode,
            venue,
            bench,
            user,
            live,
        })
    }
}

/// A path from the page must stay inside the exercise: relative, `/`
/// separated, no `.` or `..`, no empty part.
pub fn check_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("is empty");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err("must be relative, with / between parts");
    }
    if path.contains('\0') {
        return Err("contains a NUL");
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("must not have empty, . or .. parts");
    }
    Ok(())
}

/// Whether a page file is one the learner edits: under `src/`. The rest
/// (the manifest, build scripts, cargo config, check.yaml, the circuit)
/// never comes from the browser.
pub fn editable(path: &str) -> bool {
    check_path(path).is_ok() && path.starts_with("src/")
}

#[derive(Clone)]
struct Ctx {
    service: Arc<dyn Service>,
    port: u16,
    /// The origins that may start a run: the site's lesson page, and
    /// [`DEV_ORIGIN`] under `--dev`.
    origins: Arc<[String]>,
}

impl Ctx {
    fn origins(&self) -> &[String] {
        &self.origins
    }

    fn allowed_origin(&self, origin: &str) -> bool {
        self.origins.iter().any(|o| o == origin)
    }
}

/// The routes, behind the origin and host guards. `site_origin` is the
/// lesson page's origin ([`origin_of`] the site this ter talks to); `dev`
/// also trusts the fork's dev server.
pub fn router(service: Arc<dyn Service>, port: u16, site_origin: &str, dev: bool) -> Router {
    let mut origins = vec![site_origin.to_string()];
    if dev && site_origin != DEV_ORIGIN {
        origins.push(DEV_ORIGIN.to_string());
    }
    let ctx = Ctx {
        service,
        port,
        origins: origins.into(),
    };
    Router::new()
        .route("/health", get(health))
        .route("/", get(health))
        .route("/run", post(run))
        .route("/run/", post(run))
        .route("/live/{id}/events", get(live_events))
        .route("/live/{id}/extend", post(live_extend))
        .route("/live/{id}/stop", post(live_stop))
        .route("/live/{id}/pause", post(live_pause))
        .route("/live/{id}/resume", post(live_resume))
        .route("/live/{id}/reset", post(live_reset))
        .route("/live/{id}/view", get(live_view))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(ctx.clone(), guard))
        .with_state(ctx)
}

async fn health(State(ctx): State<Ctx>) -> Response {
    let mut body = json!({"service": "ter"});
    if let (Some(out), Some(extra)) = (body.as_object_mut(), ctx.service.health().as_object()) {
        for (k, v) in extra {
            out.insert(k.clone(), v.clone());
        }
    }
    axum::Json(body).into_response()
}

async fn run(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    let json_body = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
    if !json_body {
        return ServeError::new(
            "unsupported_media_type",
            "Send the run as Content-Type: application/json.",
        )
        .into_response();
    }
    let request = match RunRequest::parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    match ctx.service.run(request).await {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

/// How often a quiet event stream sends a comment line, so a page that
/// went away is noticed: a write to a peer that is gone fails only a
/// write or two after it went.
const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// A live run's events as the body of an SSE response.
struct EventStream {
    events: LiveEvents,
    beat: tokio::time::Interval,
}

impl futures_core::Stream for EventStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.events.poll_recv(cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(Ok(Bytes::from(format!(
                "event: {}\ndata: {}\n\n",
                event.name, event.data
            ))))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match self.beat.poll_tick(cx) {
                Poll::Ready(_) => Poll::Ready(Some(Ok(Bytes::from_static(b": ter\n\n")))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

async fn live_events(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    let Some(events) = ctx.service.live_events(&id) else {
        return no_live(&id).into_response();
    };
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + KEEP_ALIVE, KEEP_ALIVE);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut r = Response::new(Body::from_stream(EventStream { events, beat }));
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

async fn live_extend(State(ctx): State<Ctx>, Path(id): Path<String>, body: Bytes) -> Response {
    let add_s = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("add_s").and_then(Value::as_u64))
        .filter(|s| *s >= 1);
    let Some(add_s) = add_s else {
        return ServeError::bad_request(
            "Send {\"add_s\": 60}: a whole number of seconds, 1 or more.",
        )
        .into_response();
    };
    match ctx.service.live_extend(&id, add_s) {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn live_stop(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    match ctx.service.live_stop(&id) {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn live_control(ctx: &Ctx, id: &str, control: LiveControl) -> Response {
    match ctx.service.live_control(id, control).await {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn live_pause(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    live_control(&ctx, &id, LiveControl::Pause).await
}

async fn live_resume(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    live_control(&ctx, &id, LiveControl::Resume).await
}

async fn live_reset(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    live_control(&ctx, &id, LiveControl::Reset).await
}

/// The lesson page frames this address; the browser goes on to the
/// simulator's own page for the run. The frame's request names no origin,
/// so what guards it is the run's id, which only the page that started
/// the run was told.
async fn live_view(State(ctx): State<Ctx>, Path(id): Path<String>) -> Response {
    let service = ctx.service.clone();
    let asked = id.clone();
    let found = tokio::task::spawn_blocking(move || service.live_view(&asked))
        .await
        .unwrap_or_else(|_| Err(no_live(&id)));
    let to = match found.map(|to| HeaderValue::from_str(&to)) {
        Ok(Ok(to)) => to,
        Ok(Err(_)) => {
            return ServeError::new("venue_unavailable", "The simulator gave no page to show.")
                .into_response();
        }
        Err(e) => return e.into_response(),
    };
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::FOUND;
    let h = r.headers_mut();
    h.insert(header::LOCATION, to);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    r
}

/// `/live/<id>/view`: the one `/live` path a frame asks for, not a script.
fn is_view(method: &Method, path: &str) -> bool {
    method == Method::GET
        && path
            .strip_prefix("/live/")
            .and_then(|rest| rest.strip_suffix("/view"))
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

async fn not_found() -> Response {
    ServeError::new(
        "not_found",
        "ter serve answers /health, /run and /live/<id>/events, /extend, /pause, /resume, /reset, /stop and /view.",
    )
    .into_response()
}

/// `127.0.0.1:<port>` or `localhost:<port>`: the names this service is
/// reached by. Anything else is a page that rebound its own name here.
fn allowed_host(host: &str, port: u16) -> bool {
    let host = host.trim().to_ascii_lowercase();
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|h| host == format!("{h}:{port}") || (port == 80 && host == *h))
}

async fn guard(State(ctx): State<Ctx>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| allowed_host(h, ctx.port));
    let private_network = request
        .headers()
        .get("access-control-request-private-network")
        .is_some();

    let mut response = if !host_ok {
        ServeError::new(
            "host_refused",
            "ter serve only answers requests to 127.0.0.1 or localhost.",
        )
        .into_response()
    } else if origin.as_deref().is_some_and(|o| !ctx.allowed_origin(o)) {
        ServeError::new(
            "origin_refused",
            format!(
                "ter serve only answers the TER Learn lesson page ({}).",
                ctx.origins().join(" or ")
            ),
        )
        .into_response()
    } else if origin.is_none()
        && (matches!(path.as_str(), "/run" | "/run/") || path.starts_with("/live/"))
        && !is_view(&method, &path)
    {
        ServeError::new(
            "origin_refused",
            format!(
                "ter serve only takes runs from the TER Learn lesson page ({}), and this request names no origin. In a terminal, use `ter run`.",
                ctx.origins().join(" or ")
            ),
        )
        .into_response()
    } else if method == Method::OPTIONS {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NO_CONTENT;
        let h = r.headers_mut();
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type"),
        );
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        if private_network {
            // Chrome asks before a public page reaches a loopback address.
            h.insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
        }
        r
    } else {
        next.run(request).await
    };

    let h = response.headers_mut();
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    if host_ok
        && let Some(o) = origin.as_deref().filter(|o| ctx.allowed_origin(o))
        && let Ok(v) = HeaderValue::from_str(o)
    {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    eprintln!(
        "{method} {path} from {} -> {}",
        origin.as_deref().unwrap_or("no origin"),
        response.status().as_u16()
    );
    response
}

/// Serve on `listener` until the process ends, for the lesson page on
/// `site_origin`. `dev` also trusts the fork's dev server.
pub async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<dyn Service>,
    site_origin: &str,
    dev: bool,
) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    let app = router(service, port, site_origin, dev)
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT));
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Records what it was asked and answers with `answer`.
    struct Fake {
        asked: Mutex<Vec<RunRequest>>,
        answer: Result<Value, ServeError>,
    }

    impl Service for Fake {
        fn health(&self) -> Value {
            json!({"version": "0.1.0", "posts_to_site": true})
        }
        fn run(&self, request: RunRequest) -> RunFuture {
            self.asked.lock().unwrap().push(request);
            let answer = self.answer.clone();
            Box::pin(async move { answer })
        }
    }

    const PORT: u16 = 7357;

    fn app(answer: Result<Value, ServeError>) -> (Router, Arc<Fake>) {
        app_with(answer, false)
    }

    /// As `ter serve --dev` when `dev`, set to the live site.
    fn app_with(answer: Result<Value, ServeError>, dev: bool) -> (Router, Arc<Fake>) {
        app_for(answer, SITE_ORIGIN, dev)
    }

    /// As a ter set to the site whose lesson page is on `site_origin`.
    fn app_for(
        answer: Result<Value, ServeError>,
        site_origin: &str,
        dev: bool,
    ) -> (Router, Arc<Fake>) {
        let fake = Arc::new(Fake {
            asked: Mutex::new(Vec::new()),
            answer,
        });
        (router(fake.clone(), PORT, site_origin, dev), fake)
    }

    fn req(method: &str, path: &str, origin: Option<&str>, body: &str) -> Request {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", format!("127.0.0.1:{PORT}"))
            .header("content-type", "application/json");
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn send(app: &Router, r: Request) -> (StatusCode, HeaderMap, Value) {
        let resp = app.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, v)
    }

    /// The live site's lesson page.
    const SITE_ORIGIN: &str = "https://learn.theembeddedrustacean.com";
    const SITE: &str = SITE_ORIGIN;
    /// Another copy of the site (a made-up host).
    const OTHER: &str = "https://staging.learn.example";
    const BODY: &str = r#"{"exercise_id": "gpio-blinky--xiao-esp32c3-nostd",
        "files": [{"path": "src/bin/main.rs", "content": "fn main() {}"}]}"#;

    #[tokio::test]
    async fn health_says_what_it_is() {
        let (app, _) = app(Ok(json!({})));
        let (status, h, v) = send(&app, req("GET", "/health", Some(SITE), "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            v,
            json!({"service": "ter", "version": "0.1.0", "posts_to_site": true})
        );
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], SITE);
        assert_eq!(h[header::VARY], "Origin");
    }

    #[tokio::test]
    async fn with_dev_both_origins_are_allowed_and_preflight_is_answered() {
        let (app, _) = app_with(Ok(json!({"check_status": "passed"})), true);
        for origin in [SITE_ORIGIN, DEV_ORIGIN] {
            let mut r = req("OPTIONS", "/run", Some(origin), "");
            r.headers_mut().insert(
                "access-control-request-private-network",
                HeaderValue::from_static("true"),
            );
            r.headers_mut().insert(
                "access-control-request-method",
                HeaderValue::from_static("POST"),
            );
            let (status, h, _) = send(&app, r).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{origin}");
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], origin);
            assert!(
                h[header::ACCESS_CONTROL_ALLOW_METHODS]
                    .to_str()
                    .unwrap()
                    .contains("POST")
            );
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_HEADERS], "Content-Type");
            assert_eq!(h["access-control-allow-private-network"], "true");

            let (status, h, v) = send(&app, req("POST", "/run", Some(origin), BODY)).await;
            assert_eq!(status, StatusCode::OK, "{origin}");
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], origin);
            assert_eq!(v["check_status"], "passed");
        }
    }

    #[tokio::test]
    async fn the_dev_server_is_refused_without_dev() {
        let (app, fake) = app(Ok(json!({})));
        for (method, body) in [("POST", BODY), ("OPTIONS", "")] {
            let (status, h, v) = send(&app, req(method, "/run", Some(DEV_ORIGIN), body)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
            assert_eq!(v["error"]["code"], "origin_refused");
            assert!(
                !v["error"]["message"].as_str().unwrap().contains("8080"),
                "the dev server is not offered: {v}"
            );
            assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        }
        let (status, _, _) = send(&app, req("GET", "/health", Some(DEV_ORIGIN), "")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.asked.lock().unwrap().is_empty());

        let (status, h, _) = send(&app, req("POST", "/run", Some(SITE), BODY)).await;
        assert_eq!(status, StatusCode::OK, "the site still runs");
        assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], SITE);
    }

    #[tokio::test]
    async fn a_ter_set_to_another_site_answers_that_sites_page_and_not_the_live_one() {
        for dev in [false, true] {
            let (app, fake) = app_for(Ok(json!({"check_status": "passed"})), OTHER, dev);
            let mut r = req("OPTIONS", "/run", Some(OTHER), "");
            r.headers_mut().insert(
                "access-control-request-private-network",
                HeaderValue::from_static("true"),
            );
            let (status, h, _) = send(&app, r).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "dev {dev}");
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], OTHER);
            assert_eq!(h["access-control-allow-private-network"], "true");
            let (status, h, v) = send(&app, req("POST", "/run", Some(OTHER), BODY)).await;
            assert_eq!(status, StatusCode::OK, "dev {dev}");
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], OTHER);
            assert_eq!(v["check_status"], "passed");
            assert_eq!(fake.asked.lock().unwrap().len(), 1);

            for (method, body) in [("POST", BODY), ("OPTIONS", "")] {
                let (status, h, v) = send(&app, req(method, "/run", Some(SITE), body)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{method}, dev {dev}");
                assert_eq!(v["error"]["code"], "origin_refused");
                let said = v["error"]["message"].as_str().unwrap();
                assert!(said.contains(OTHER), "says which page it answers: {said}");
                assert!(!said.contains(SITE), "{said}");
                assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
            }
            let (status, _, _) = send(&app, req("GET", "/health", Some(SITE), "")).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(fake.asked.lock().unwrap().len(), 1, "nothing more ran");

            // The dev server follows --dev, whatever the site.
            let (status, h, _) = send(&app, req("POST", "/run", Some(DEV_ORIGIN), BODY)).await;
            if dev {
                assert_eq!(status, StatusCode::OK);
                assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], DEV_ORIGIN);
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
            }

            // The other guards hold as they do for the live site.
            let (status, _, v) = send(&app, req("POST", "/run", None, BODY)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(v["error"]["code"], "origin_refused");
            let mut r = req("POST", "/run", Some(OTHER), BODY);
            r.headers_mut()
                .insert("host", HeaderValue::from_static("staging.learn.example"));
            let (status, _, v) = send(&app, r).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(v["error"]["code"], "host_refused");
        }
    }

    #[test]
    fn a_sites_origin_is_its_scheme_host_and_port() {
        for (site, origin) in [
            (SITE_ORIGIN, SITE_ORIGIN),
            ("https://learn.theembeddedrustacean.com/", SITE_ORIGIN),
            ("https://Staging.Learn.Example/app/lms?x=1#y", OTHER),
            ("https://staging.learn.example:443", OTHER),
            (
                "https://staging.learn.example:8443/",
                "https://staging.learn.example:8443",
            ),
            ("https://user:secret@staging.learn.example/", OTHER),
            ("http://127.0.0.1:8000/", "http://127.0.0.1:8000"),
            ("http://localhost:80", "http://localhost"),
            (" http://localhost:8080 ", DEV_ORIGIN),
        ] {
            assert_eq!(origin_of(site).as_deref(), Some(origin), "{site}");
        }
        for not_a_site in [
            "",
            "staging.learn.example",
            "file:///tmp/site",
            "ftp://x.example",
            "https://",
        ] {
            assert_eq!(origin_of(not_a_site), None, "{not_a_site:?}");
        }
    }

    #[tokio::test]
    async fn a_run_with_no_origin_is_refused_and_nothing_runs() {
        for dev in [false, true] {
            let (app, fake) = app_with(Ok(json!({})), dev);
            for path in ["/run", "/run/"] {
                let (status, h, v) = send(&app, req("POST", path, None, BODY)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path}, dev {dev}");
                assert_eq!(v["error"]["code"], "origin_refused");
                assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
            }
            assert!(fake.asked.lock().unwrap().is_empty());
            // A health probe from a terminal names no origin and is harmless.
            let (status, _, v) = send(&app, req("GET", "/health", None, "")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(v["service"], "ter");
        }
    }

    #[tokio::test]
    async fn any_other_origin_is_refused_and_nothing_runs() {
        let (app, fake) = app(Ok(json!({})));
        for origin in [
            "https://evil.example",
            "http://learn.theembeddedrustacean.com",
            "https://learn.theembeddedrustacean.com.evil.example",
            "http://localhost:8081",
            "null",
        ] {
            let (status, h, v) = send(&app, req("POST", "/run", Some(origin), BODY)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
            assert_eq!(v["error"]["code"], "origin_refused");
            assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
            let (status, h, _) = send(&app, req("OPTIONS", "/run", Some(origin), "")).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
            assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        }
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rebound_host_is_refused() {
        let (app, fake) = app(Ok(json!({})));
        let mut r = req("POST", "/run", Some(SITE), BODY);
        r.headers_mut()
            .insert("host", HeaderValue::from_static("attacker.example:7357"));
        let (status, h, v) = send(&app, r).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "host_refused");
        assert!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert!(fake.asked.lock().unwrap().is_empty());
        let mut r = req("GET", "/health", None, "");
        r.headers_mut()
            .insert("host", HeaderValue::from_static("localhost:7357"));
        assert_eq!(send(&app, r).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_run_must_be_json() {
        let (app, fake) = app(Ok(json!({})));
        let mut r = req("POST", "/run", Some(SITE), BODY);
        r.headers_mut()
            .insert("content-type", HeaderValue::from_static("text/plain"));
        let (status, _, v) = send(&app, r).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(v["error"]["code"], "unsupported_media_type");
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_request_reaches_the_service_with_no_mode_when_the_page_gives_none() {
        let (app, fake) = app(Ok(json!({})));
        let (status, _, _) = send(&app, req("POST", "/run", Some(SITE), BODY)).await;
        assert_eq!(status, StatusCode::OK);
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked[0].exercise_id, "gpio-blinky--xiao-esp32c3-nostd");
        assert_eq!(asked[0].mode, None);
        assert_eq!(asked[0].user, None);
        assert_eq!(asked[0].files[0].path, "src/bin/main.rs");
    }

    #[tokio::test]
    async fn a_run_that_was_not_posted_is_an_error_status() {
        let (app, _) = app(Err(ServeError::new(
            "wrong_account",
            "paired with someone else",
        )));
        let (status, h, v) = send(&app, req("POST", "/run", Some(SITE), BODY)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "wrong_account");
        assert_eq!(
            h[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            SITE,
            "the page can read why"
        );
    }

    #[test]
    fn bodies_are_validated() {
        let bad = |body: &str| RunRequest::parse(body.as_bytes()).unwrap_err().message;
        assert!(bad("not json").contains("not JSON"));
        assert!(bad("[]").contains("object"));
        assert!(bad("{}").contains("exercise_id"));
        assert!(bad(r#"{"exercise_id": "  "}"#).contains("exercise_id"));
        assert!(bad(r#"{"exercise_id": "a", "files": {}}"#).contains("list"));
        assert!(
            bad(r#"{"exercise_id": "a", "files": [{"path": "src/a.rs"}]}"#)
                .contains("files[0].content")
        );
        assert!(
            bad(r#"{"exercise_id": "a", "files": [{"path": "../x", "content": ""}]}"#)
                .contains("files[0].path")
        );
        assert!(bad(r#"{"exercise_id": "a", "mode": "remote"}"#).contains("mode"));
        assert!(bad(r#"{"exercise_id": "a", "user": 3}"#).contains("user"));

        let ok = RunRequest::parse(
            br#"{"exercise_id": " a ", "mode": "hardware", "user": "me@example.com", "extra": 1}"#,
        )
        .unwrap();
        assert_eq!(ok.exercise_id, "a");
        assert_eq!(ok.mode.as_deref(), Some("hardware"));
        assert_eq!(ok.user.as_deref(), Some("me@example.com"));
        assert!(ok.files.is_empty());
        let blank_user = RunRequest::parse(br#"{"exercise_id": "a", "user": ""}"#).unwrap();
        assert_eq!(blank_user.user, None);
        assert_eq!(blank_user.venue, None, "no venue is today's behaviour");
        assert_eq!(blank_user.bench, None);
    }

    #[test]
    fn a_venue_and_a_bench_are_read_and_checked() {
        let parse = |body: &str| RunRequest::parse(body.as_bytes());
        let bad = |body: &str| parse(body).unwrap_err().message;

        let r = parse(r#"{"exercise_id": "a", "mode": "hardware", "venue": "bench"}"#).unwrap();
        assert_eq!(
            (r.mode.as_deref().unwrap(), r.venue.as_deref(), r.bench),
            ("hardware", Some("bench"), None)
        );
        let r = parse(
            r#"{"exercise_id": "a", "mode": "hardware", "venue": "bench", "bench": " b-1 "}"#,
        )
        .unwrap();
        assert_eq!(r.bench.as_deref(), Some("b-1"));
        let r = parse(r#"{"exercise_id": "a", "mode": "hardware", "venue": "local"}"#).unwrap();
        assert_eq!(r.venue.as_deref(), Some("local"));
        // A venue or a bench alone means hardware; a bench alone, the bench.
        let r = parse(r#"{"exercise_id": "a", "venue": "local"}"#).unwrap();
        assert_eq!(r.mode.as_deref(), Some("hardware"));
        let r = parse(r#"{"exercise_id": "a", "bench": "b-1"}"#).unwrap();
        assert_eq!(
            (r.mode.as_deref().unwrap(), r.venue.as_deref()),
            ("hardware", Some("bench"))
        );
        let r = parse(r#"{"exercise_id": "a", "venue": null, "bench": ""}"#).unwrap();
        assert_eq!(
            (r.mode, r.venue, r.bench),
            (None, None, None),
            "no mode named: the service picks the exercise's"
        );

        assert!(bad(r#"{"exercise_id": "a", "venue": "renode"}"#).contains("venue must be"));
        assert!(bad(r#"{"exercise_id": "a", "venue": 1}"#).contains("venue must be a string"));
        assert!(bad(r#"{"exercise_id": "a", "bench": 1}"#).contains("bench must be a string"));
        assert!(
            bad(r#"{"exercise_id": "a", "mode": "simulation", "venue": "bench"}"#)
                .contains("hardware")
        );
        assert!(bad(r#"{"exercise_id": "a", "venue": "local", "bench": "b-1"}"#).contains("bench"));
    }

    #[test]
    fn a_simulator_is_the_venue_of_a_simulation() {
        let parse = |body: &str| RunRequest::parse(body.as_bytes());
        let bad = |body: &str| parse(body).unwrap_err().message;

        for sim in SIMULATORS {
            let r = parse(&format!(
                r#"{{"exercise_id": "a", "mode": "simulation", "venue": "{sim}"}}"#
            ))
            .unwrap();
            assert_eq!(
                (r.mode.as_deref(), r.venue.as_deref(), r.bench),
                (Some("simulation"), Some(sim), None)
            );
            // A simulator alone means a simulation.
            let r = parse(&format!(r#"{{"exercise_id": "a", "venue": "{sim}"}}"#)).unwrap();
            assert_eq!(r.mode.as_deref(), Some("simulation"));
        }
        // A simulation that names none is the exercise's own, as before.
        let r = parse(r#"{"exercise_id": "a", "mode": "simulation"}"#).unwrap();
        assert_eq!((r.mode.as_deref(), r.venue), (Some("simulation"), None));

        let m = bad(r#"{"exercise_id": "a", "mode": "hardware", "venue": "sim86"}"#);
        assert!(m.contains("\"sim86\" is a simulator"), "{m}");
        let m = bad(r#"{"exercise_id": "a", "mode": "simulation", "venue": "local"}"#);
        assert!(m.contains("wokwi, sim86"), "{m}");
        let m = bad(r#"{"exercise_id": "a", "venue": "sim86", "bench": "b-1"}"#);
        assert!(m.contains("not \"sim86\""), "{m}");
    }

    #[test]
    fn bench_errors_have_their_statuses() {
        let status = |code: &str| ServeError::new(code, "m").status();
        assert_eq!(status("bench_busy"), StatusCode::CONFLICT);
        assert_eq!(status("bench_offline"), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status("no_capacity"), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status("wrong_board"), StatusCode::BAD_REQUEST);
        assert_eq!(status("unknown_bench"), StatusCode::NOT_FOUND);
        assert_eq!(status("not_observable"), StatusCode::BAD_REQUEST);
        assert_eq!(status("timeout"), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(status("replaced"), StatusCode::CONFLICT);
    }

    #[test]
    fn paths_stay_inside_and_only_src_is_editable() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "src/../../x",
            "src//a.rs",
            "./src/a.rs",
            "C:\\x",
            "src\\a.rs",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
            assert!(!editable(bad), "{bad:?}");
        }
        assert!(editable("src/bin/main.rs"));
        assert!(editable("src/lib.rs"));
        for kept in [
            "Cargo.toml",
            "build.rs",
            ".cargo/config.toml",
            "check.yaml",
            "diagram.json",
            "ter.toml",
            "srcx/a.rs",
        ] {
            assert!(check_path(kept).is_ok(), "{kept}");
            assert!(!editable(kept), "{kept}");
        }
    }

    #[test]
    fn a_run_can_be_asked_for_live_with_a_limit_in_seconds() {
        let parse = |live: &str| {
            RunRequest::parse(
                format!(r#"{{"exercise_id": "e", "venue": "sim86", "live": {live}}}"#).as_bytes(),
            )
        };
        assert_eq!(parse(r#"{"limit_s": 90}"#).unwrap().live, Some(90));
        assert_eq!(parse("{}").unwrap().live, Some(LIVE_LIMIT_S));
        assert_eq!(parse("null").unwrap().live, None);
        assert_eq!(RunRequest::parse(BODY.as_bytes()).unwrap().live, None);
        for bad in [
            r#"{"limit_s": 0}"#,
            r#"{"limit_s": 601}"#,
            r#"{"limit_s": "60"}"#,
            "true",
        ] {
            assert_eq!(parse(bad).unwrap_err().code, "bad_request", "{bad}");
        }
        // Hardware answers as before; it has no live run yet.
        let hw = RunRequest::parse(br#"{"exercise_id": "e", "mode": "hardware", "live": {}}"#);
        assert_eq!(hw.unwrap_err().code, "bad_request");
    }

    /// A service with one live run, `l1`, that sends two events and ends.
    struct Watched;

    impl Service for Watched {
        fn health(&self) -> Value {
            json!({"live": true})
        }
        fn run(&self, _: RunRequest) -> RunFuture {
            Box::pin(async { Ok(json!({"live": "l1"})) })
        }
        fn live_events(&self, id: &str) -> Option<LiveEvents> {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            for (name, data) in [
                ("pin", json!({"t_ms": 1.5, "pin": "PA5", "level": 1})),
                ("ended", json!({"reason": "limit", "detail": ""})),
            ] {
                tx.try_send(LiveEvent { name, data }).unwrap();
            }
            (id == "l1").then_some(rx)
        }
        fn live_extend(&self, id: &str, add_s: u64) -> Result<Value, ServeError> {
            match id {
                "l1" if add_s > 60 => Err(ServeError::new("limit_cap", "ten minutes at most")),
                "l1" => Ok(json!({"ends_at": add_s})),
                _ => Err(no_live(id)),
            }
        }
        fn live_view(&self, id: &str) -> Result<String, ServeError> {
            (id == "l1")
                .then(|| "https://sim.example/live/r1#key=k".to_string())
                .ok_or_else(|| no_live(id))
        }
        fn live_control(&self, id: &str, control: LiveControl) -> RunFuture {
            let answer = match (id, control) {
                ("l1", LiveControl::Pause) => Ok(json!({"ok": true, "t_ms": 1.5})),
                ("l1", LiveControl::Resume) => Ok(json!({"ok": true, "t_ms": 1.5, "ends_at": 9})),
                ("l1", LiveControl::Reset) => Ok(json!({"ok": true, "run": "r2"})),
                (id, _) => Err(no_live(id)),
            };
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn a_live_run_is_followed_extended_and_stopped_by_the_lesson_page_only() {
        let app = router(Arc::new(Watched), PORT, SITE_ORIGIN, false);

        let resp = app
            .clone()
            .oneshot(req("GET", "/live/l1/events", Some(SITE), ""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], SITE);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "event: pin\ndata: {\"level\":1,\"pin\":\"PA5\",\"t_ms\":1.5}\n\n\
             event: ended\ndata: {\"detail\":\"\",\"reason\":\"limit\"}\n\n"
        );

        let (status, _, v) = send(
            &app,
            req("POST", "/live/l1/extend", Some(SITE), r#"{"add_s": 60}"#),
        )
        .await;
        assert_eq!((status, v), (StatusCode::OK, json!({"ends_at": 60})));
        let (status, _, v) = send(
            &app,
            req("POST", "/live/l1/extend", Some(SITE), r#"{"add_s": 61}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], "limit_cap");
        for body in ["", "{}", r#"{"add_s": 0}"#, r#"{"add_s": -5}"#] {
            let (status, _, v) = send(&app, req("POST", "/live/l1/extend", Some(SITE), body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {v}");
        }

        // The board is held and let go once the simulator has done it.
        let (status, _, v) = send(&app, req("POST", "/live/l1/pause", Some(SITE), "")).await;
        assert_eq!(
            (status, v),
            (StatusCode::OK, json!({"ok": true, "t_ms": 1.5}))
        );
        let (status, _, v) = send(&app, req("POST", "/live/l1/resume", Some(SITE), "{}")).await;
        assert_eq!(
            (status, v),
            (
                StatusCode::OK,
                json!({"ok": true, "t_ms": 1.5, "ends_at": 9})
            )
        );
        let (status, _, v) = send(&app, req("POST", "/live/l1/reset", Some(SITE), "")).await;
        assert_eq!(
            (status, v),
            (StatusCode::OK, json!({"ok": true, "run": "r2"}))
        );

        // No such run, and a service with no live runs at all.
        for path in [
            "/live/l2/events",
            "/live/l2/extend",
            "/live/l1/stop",
            "/live/l2/pause",
            "/live/l2/resume",
            "/live/l2/reset",
        ] {
            let method = if path.ends_with("events") {
                "GET"
            } else {
                "POST"
            };
            let (status, _, v) = send(&app, req(method, path, Some(SITE), r#"{"add_s": 1}"#)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {v}");
            assert_eq!(v["error"]["code"], "not_found");
        }

        // The browser asks first, as for /run: every /live path answers.
        for path in [
            "/live/l1/events",
            "/live/l1/extend",
            "/live/l1/stop",
            "/live/l1/pause",
            "/live/l1/resume",
            "/live/l1/reset",
        ] {
            let mut r = req("OPTIONS", path, Some(SITE), "");
            r.headers_mut().insert(
                "access-control-request-private-network",
                HeaderValue::from_static("true"),
            );
            let (status, h, _) = send(&app, r).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{path}");
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], SITE);
            assert_eq!(h[header::ACCESS_CONTROL_ALLOW_HEADERS], "Content-Type");
            assert_eq!(h["access-control-allow-private-network"], "true");
        }

        // The frame in the lesson page is sent on to the simulator's own
        // page. A frame names no origin; a script of another page is
        // refused as everywhere, and cannot read where a frame went.
        for origin in [None, Some(SITE)] {
            let (status, h, _) = send(&app, req("GET", "/live/l1/view", origin, "")).await;
            assert_eq!(status, StatusCode::FOUND, "{origin:?}");
            assert_eq!(h[header::LOCATION], "https://sim.example/live/r1#key=k");
            assert_eq!(h[header::CACHE_CONTROL], "no-store");
            assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        }
        let (status, _, v) = send(&app, req("GET", "/live/l2/view", None, "")).await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::NOT_FOUND, &json!("not_found"))
        );
        let (status, _, v) = send(&app, req("GET", "/live/l1/view", Some(OTHER), "")).await;
        assert_eq!(
            (status, &v["error"]["code"]),
            (StatusCode::FORBIDDEN, &json!("origin_refused"))
        );
        let (status, _, _) = send(&app, req("POST", "/live/l1/view", None, "")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "only a GET goes without an origin"
        );

        // Another page, or no page at all, gets nothing and stops nothing.
        for origin in [Some(OTHER), None] {
            for (method, path) in [
                ("GET", "/live/l1/events"),
                ("POST", "/live/l1/stop"),
                ("POST", "/live/l1/pause"),
                ("POST", "/live/l1/resume"),
                ("POST", "/live/l1/reset"),
            ] {
                let (status, _, v) = send(&app, req(method, path, origin, "")).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?} {path}");
                assert_eq!(v["error"]["code"], "origin_refused");
            }
        }
    }
}
