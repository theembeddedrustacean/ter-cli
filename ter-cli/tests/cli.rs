//! The `ter` binary against a local mock of the site.

use std::process::{Command, Output};

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";

fn ter(site: &str, token: Option<&str>, args: &[&str]) -> Output {
    let config = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ter"));
    cmd.args(args)
        .env("TER_SITE_URL", site)
        .env("TER_CONFIG_DIR", config.path())
        .env_remove("TER_TOKEN");
    if let Some(t) = token {
        cmd.env("TER_TOKEN", t);
    }
    cmd.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

async fn mock_site(min_version: &str) -> MockServer {
    mock_site_with(min_version, json!(true)).await
}

async fn mock_site_with(min_version: &str, premium: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
            "ok": true,
            "user": "learner@example.com",
            "server_time": "2026-09-27T12:00:00",
            "min_supported_version": min_version,
            "latest_version": "9.9.9",
            "download_url": null,
            "premium": premium
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
            "courses": [{"course_id": "esp-gpio", "title": "ESP GPIO", "lessons": []}]
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": {"error": {
                "code": "token_invalid",
                "message": "No valid device token on this request."
            }}})),
        )
        .mount(&server)
        .await;
    server
}

#[test]
fn version_flag() {
    let o = ter("http://127.0.0.1:9", None, &["--version"]);
    assert!(o.status.success());
    assert_eq!(
        stdout(&o).trim(),
        format!("ter {}", env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn whoami_prints_user_courses_and_versions() {
    let site = mock_site("0.1.0").await;
    let o = ter(&site.uri(), Some("good-token"), &["whoami"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("learner@example.com"), "{out}");
    assert!(out.contains("ESP GPIO (esp-gpio)"), "{out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
    assert!(out.contains("TER_TOKEN"), "{out}");
    assert!(out.contains("premium  yes"), "{out}");
    assert!(
        !out.contains("good-token"),
        "the token must never be printed"
    );
    assert!(stderr(&o).is_empty(), "{}", stderr(&o));
}

#[tokio::test]
async fn whoami_json() {
    let site = mock_site("0.1.0").await;
    let o = ter(&site.uri(), Some("good-token"), &["whoami", "--json"]);
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["user"], "learner@example.com");
    assert_eq!(v["supported"], true);
    assert_eq!(v["premium"], true);
    assert_eq!(v["courses"][0]["course_id"], "esp-gpio");
    assert!(!stdout(&o).contains("good-token"));
}

#[tokio::test]
async fn whoami_below_minimum_warns_and_names_self_update() {
    let site = mock_site("99.0.0").await;
    let o = ter(&site.uri(), Some("good-token"), &["whoami"]);
    assert!(o.status.success());
    assert!(stderr(&o).contains("ter self-update"), "{}", stderr(&o));

    let o = ter(&site.uri(), Some("good-token"), &["whoami", "--json"]);
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["supported"], false);
}

#[tokio::test]
async fn whoami_bad_token_exits_with_token_invalid() {
    let site = mock_site("0.1.0").await;
    let o = ter(&site.uri(), Some("bad-token"), &["whoami"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("error[token_invalid]"),
        "{}",
        stderr(&o)
    );

    let o = ter(&site.uri(), Some("bad-token"), &["whoami", "--json"]);
    assert!(!o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["error"]["code"], "token_invalid");
    assert_eq!(
        v["error"]["message"],
        "No valid device token on this request."
    );
}

#[test]
fn whoami_without_token_is_no_token() {
    let o = ter("http://127.0.0.1:9", None, &["whoami", "--json"]);
    assert!(!o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["error"]["code"], "no_token");
}

#[test]
fn bad_config_file_is_config_error() {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(config.path().join("config.toml"), "not toml [").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["whoami", "--json"])
        .env("TER_CONFIG_DIR", config.path())
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["error"]["code"], "config_error");
}

#[test]
fn self_update_prints_a_reinstall_command() {
    let cargo_home = tempfile::tempdir().unwrap();
    std::fs::write(
        cargo_home.path().join(".crates2.json"),
        json!({"installs": {"ter-cli 0.1.0 (git+https://example.com/ter-cli.git#abc)": {}}})
            .to_string(),
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["self-update", "--json"])
        .env("CARGO_HOME", cargo_home.path())
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(
        v["command"],
        "cargo install --git https://example.com/ter-cli.git ter-cli --force"
    );
}

#[tokio::test]
async fn whoami_shows_when_the_account_is_not_premium() {
    let site = mock_site_with("0.1.0", json!(false)).await;
    let o = ter(&site.uri(), Some("good-token"), &["whoami"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("premium  no"), "{}", stdout(&o));
}

#[tokio::test]
async fn whoami_from_a_site_without_premium_says_unknown() {
    let site = mock_site_with("0.1.0", Value::Null).await;
    let o = ter(&site.uri(), Some("good-token"), &["whoami", "--json"]);
    assert!(o.status.success());
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["premium"], Value::Null);
}
