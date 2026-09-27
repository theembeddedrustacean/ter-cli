//! `ter login`, `ter logout` and a revoked token, against a local mock of
//! the site. The keychain is off, so the token goes to the file in a
//! temporary config directory.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";
const CODE: &str = "WXYZ-1234";
const TOKEN: &str = "fresh-device-token";

fn ter(config: &Path, site: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(args)
        .env("TER_SITE_URL", site)
        .env("TER_CONFIG_DIR", config)
        .env("TER_KEYCHAIN", "off")
        .env_remove("TER_TOKEN")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn assert_no_token(o: &Output) {
    assert!(!out(o).contains(TOKEN), "token printed: {}", out(o));
    assert!(!err(o).contains(TOKEN), "token printed: {}", err(o));
}

async fn mount_ping(server: &MockServer, token: &str) {
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
            "ok": true,
            "user": "learner@example.com",
            "server_time": "2026-09-27T12:00:00",
            "min_supported_version": "0.1.0",
            "latest_version": env!("CARGO_PKG_VERSION"),
            "download_url": null,
            "premium": true
        }})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {"courses": []}})))
        .mount(server)
        .await;
}

async fn mount_pair_start(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("{API}.pair_start")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"message": {"code": CODE, "expires_in": 600}})),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn poll(status: Value) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("{API}.pair_poll")))
        .and(query_param("code", CODE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": status })))
}

/// A site that says `pending` `pending_polls` times, then approves.
async fn pairing_site(pending_polls: u64) -> MockServer {
    let server = MockServer::start().await;
    mount_pair_start(&server).await;
    if pending_polls > 0 {
        poll(json!({"status": "pending"}))
            .up_to_n_times(pending_polls)
            .with_priority(1)
            .expect(pending_polls)
            .mount(&server)
            .await;
    }
    poll(json!({"status": "approved", "token": TOKEN}))
        .up_to_n_times(1)
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    poll(json!({"status": "expired"}))
        .with_priority(3)
        .mount(&server)
        .await;
    mount_ping(&server, TOKEN).await;
    server
}

#[tokio::test]
async fn login_pairs_stores_the_token_and_whoami_uses_it() {
    let site = pairing_site(1).await;
    let config = tempfile::tempdir().unwrap();

    let o = ter(config.path(), &site.uri(), &["login"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_no_token(&o);
    let text = out(&o);
    assert!(text.contains(&format!("\n    {CODE}\n")), "{text}");
    assert!(
        text.contains(&format!("{}/cli?code={CODE}", site.uri())),
        "{text}"
    );
    assert!(text.contains("Logged in as learner@example.com"), "{text}");
    assert!(
        err(&o).contains("readable only by you"),
        "the file fallback must say so: {}",
        err(&o)
    );

    let file = config.path().join("token");
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), TOKEN);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let o = ter(config.path(), &site.uri(), &["whoami", "--json"]);
    assert!(o.status.success(), "{}", out(&o));
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["user"], "learner@example.com");
    assert_eq!(v["token_source"], "file");
    assert_no_token(&o);
}

#[tokio::test]
async fn login_json_keeps_stdout_to_the_result() {
    let site = pairing_site(0).await;
    let config = tempfile::tempdir().unwrap();

    let o = ter(config.path(), &site.uri(), &["login", "--json"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_no_token(&o);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["user"], "learner@example.com");
    assert_eq!(v["stored_in"], "file");
    assert!(v["token_file"].as_str().unwrap().ends_with("token"));
    assert!(err(&o).contains(CODE), "the prompt goes to stderr");
}

#[tokio::test]
async fn an_expired_code_stores_nothing() {
    let server = MockServer::start().await;
    mount_pair_start(&server).await;
    poll(json!({"status": "expired"})).mount(&server).await;
    let config = tempfile::tempdir().unwrap();

    let o = ter(config.path(), &server.uri(), &["login", "--json"]);
    assert!(!o.status.success());
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["error"]["code"], "pairing_expired");
    assert!(!config.path().join("token").exists());
}

#[tokio::test]
async fn pair_start_rate_limited_exits_with_the_site_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.pair_start")))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"message": {"error": {
                "code": "rate_limited", "message": "Too many pairing codes. Try again later."
            }}})),
        )
        .mount(&server)
        .await;
    let config = tempfile::tempdir().unwrap();
    let o = ter(config.path(), &server.uri(), &["login"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("error[rate_limited]"), "{}", err(&o));
}

#[tokio::test]
async fn logout_removes_the_token() {
    let site = pairing_site(0).await;
    let config = tempfile::tempdir().unwrap();
    assert!(ter(config.path(), &site.uri(), &["login"]).status.success());

    let o = ter(config.path(), &site.uri(), &["logout", "--json"]);
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["removed_from"], json!(["file"]));
    assert!(!config.path().join("token").exists());

    let o = ter(config.path(), &site.uri(), &["whoami", "--json"]);
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["error"]["code"], "no_token");

    let o = ter(config.path(), &site.uri(), &["logout"]);
    assert!(o.status.success());
    assert!(out(&o).contains("not logged in"), "{}", out(&o));
}

async fn site_rejecting(token: &str, message: &str) -> MockServer {
    site_answering_401(
        token,
        json!({"message": {"error": {"code": "token_invalid", "message": message}}}),
    )
    .await
}

/// A revoked or expired token, the way the live site reports it: Frappe's
/// own `AuthenticationError`, with the text in `_server_messages`.
async fn site_raising(token: &str, message: &str) -> MockServer {
    let inner = json!({"message": message, "indicator": "red", "raise_exception": 1});
    let list = serde_json::to_string(&vec![inner.to_string()]).unwrap();
    site_answering_401(
        token,
        json!({"exc_type": "AuthenticationError", "_server_messages": list}),
    )
    .await
}

async fn site_answering_401(token: &str, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(ResponseTemplate::new(401).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn store_token(config: &Path, token: &str) {
    std::fs::write(config.join("token"), format!("{token}\n")).unwrap();
}

#[tokio::test]
async fn a_revoked_token_is_removed_and_the_learner_told() {
    for (message, raised) in [
        ("Token revoked.", true),
        ("Token expired.", true),
        ("Token revoked.", false),
    ] {
        let site = if raised {
            site_raising("dead-token", message).await
        } else {
            site_rejecting("dead-token", message).await
        };
        let config = tempfile::tempdir().unwrap();
        store_token(config.path(), "dead-token");

        let o = ter(config.path(), &site.uri(), &["whoami"]);
        assert!(!o.status.success());
        let e = err(&o);
        assert!(
            e.starts_with(&format!("error[token_invalid]: {message}")),
            "{e}"
        );
        assert!(e.contains("Removed it from this machine"), "{e}");
        assert!(e.contains("ter login"), "{e}");
        assert!(!config.path().join("token").exists(), "{message}");
    }
}

#[tokio::test]
async fn an_unknown_token_is_kept() {
    let site = site_rejecting("typo-token", "No valid device token on this request.").await;
    let config = tempfile::tempdir().unwrap();
    store_token(config.path(), "typo-token");

    let o = ter(config.path(), &site.uri(), &["whoami"]);
    assert!(!o.status.success());
    assert!(!err(&o).contains("Removed"), "{}", err(&o));
    assert!(config.path().join("token").exists());
}

#[tokio::test]
async fn a_revoked_ter_token_leaves_the_stored_token_alone() {
    let site = site_rejecting("env-token", "Token revoked.").await;
    let config = tempfile::tempdir().unwrap();
    store_token(config.path(), "stored-token");

    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .arg("whoami")
        .env("TER_SITE_URL", site.uri())
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_KEYCHAIN", "off")
        .env("TER_TOKEN", "env-token")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(!err(&o).contains("Removed"), "{}", err(&o));
    assert!(config.path().join("token").exists());
}
