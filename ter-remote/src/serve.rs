//! `ter serve`: the localhost service the lesson page's editor calls.
//!
//! The page (on the TER Learn site, or the fork's dev server) posts the
//! learner's files to `http://127.0.0.1:<port>/run`; the service builds,
//! runs and checks them, posts the run with the machine's own token and
//! answers with the run it posted. This module is the HTTP side: routes,
//! CORS, the origin and host guards and body validation. What a run does
//! is the [`Service`] ter-cli plugs in.
//!
//! Only the two allowed origins get an answer a browser will read, and any
//! other origin is refused before anything runs: a POST from an unknown
//! page must not build or post, whatever CORS lets it read. A `Host` that
//! is not this loopback address is refused too, so a site that rebinds its
//! own name to 127.0.0.1 cannot reach the service.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Value, json};

/// The lesson page on the site, and the fork's dev server.
pub const ALLOWED_ORIGINS: [&str; 2] = [
    "https://learn.theembeddedrustacean.com",
    "http://localhost:8080",
];

/// The largest body `/run` takes: an exercise's files, with room to spare.
pub const BODY_LIMIT: usize = 4 * 1024 * 1024;

/// One file from the page's editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageFile {
    pub path: String,
    pub content: String,
}

/// A validated `/run` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub exercise_id: String,
    pub files: Vec<PageFile>,
    /// `simulation` when the page does not say: the editor is the
    /// simulation path.
    pub mode: String,
    /// The page's signed-in user, when the page sends it.
    pub user: Option<String>,
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
            "bad_request" | "mode_not_allowed" | "not_observable" | "bad_check" => {
                StatusCode::BAD_REQUEST
            }
            "no_token" | "not_paired" | "token_invalid" | "token_revoked" | "token_expired" => {
                StatusCode::UNAUTHORIZED
            }
            "wrong_account" | "not_enrolled" | "locked" | "origin_refused" | "host_refused" => {
                StatusCode::FORBIDDEN
            }
            "not_found" => StatusCode::NOT_FOUND,
            "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "timeout" => StatusCode::GATEWAY_TIMEOUT,
            "busy" => StatusCode::CONFLICT,
            "outdated" => StatusCode::UPGRADE_REQUIRED,
            "venue_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
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
        let mode = match obj.get("mode") {
            None | Some(Value::Null) => "simulation".to_string(),
            Some(Value::String(m)) if m == "simulation" || m == "hardware" => m.clone(),
            Some(other) => {
                return Err(ServeError::bad_request(format!(
                    "mode must be \"simulation\" or \"hardware\", not {other}."
                )));
            }
        };
        let user = match obj.get("user") {
            None | Some(Value::Null) => None,
            Some(Value::String(u)) if u.trim().is_empty() => None,
            Some(Value::String(u)) => Some(u.trim().to_string()),
            Some(_) => return Err(ServeError::bad_request("user must be a string.")),
        };
        Ok(Self {
            exercise_id,
            files,
            mode,
            user,
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
}

/// The routes, behind the origin and host guards.
pub fn router(service: Arc<dyn Service>, port: u16) -> Router {
    let ctx = Ctx { service, port };
    Router::new()
        .route("/health", get(health))
        .route("/", get(health))
        .route("/run", post(run))
        .route("/run/", post(run))
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

async fn not_found() -> Response {
    ServeError::new("not_found", "ter serve answers /health and /run.").into_response()
}

fn allowed_origin(origin: &str) -> bool {
    ALLOWED_ORIGINS.contains(&origin)
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
    } else if origin.as_deref().is_some_and(|o| !allowed_origin(o)) {
        ServeError::new(
            "origin_refused",
            format!(
                "ter serve only answers the TER Learn lesson page ({}).",
                ALLOWED_ORIGINS.join(" or ")
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
        && let Some(o) = origin.as_deref().filter(|o| allowed_origin(o))
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

/// Serve on `listener` until the process ends.
pub async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<dyn Service>,
) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    let app = router(service, port).layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT));
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
        let fake = Arc::new(Fake {
            asked: Mutex::new(Vec::new()),
            answer,
        });
        (router(fake.clone(), PORT), fake)
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

    const SITE: &str = "https://learn.theembeddedrustacean.com";
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
    async fn both_origins_are_allowed_and_preflight_is_answered() {
        let (app, _) = app(Ok(json!({"check_status": "passed"})));
        for origin in ALLOWED_ORIGINS {
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
    async fn the_request_reaches_the_service_with_simulation_by_default() {
        let (app, fake) = app(Ok(json!({})));
        let (status, _, _) = send(&app, req("POST", "/run", None, BODY)).await;
        assert_eq!(status, StatusCode::OK);
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked[0].exercise_id, "gpio-blinky--xiao-esp32c3-nostd");
        assert_eq!(asked[0].mode, "simulation");
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
        assert_eq!(ok.mode, "hardware");
        assert_eq!(ok.user.as_deref(), Some("me@example.com"));
        assert!(ok.files.is_empty());
        let blank_user = RunRequest::parse(br#"{"exercise_id": "a", "user": ""}"#).unwrap();
        assert_eq!(blank_user.user, None);
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
}
