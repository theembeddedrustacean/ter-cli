//! `ter run`, `ter hint` and `ter status` against a local mock of the site,
//! building a small host program in a temporary courses root.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";
const COURSE: &str = "esp-gpio";
const EXERCISE: &str = "gpio-blinky--xiao-esp32c3-nostd";
const USER: &str = "learner@example.com";

const GOOD: &str = "fn main() {\n    println!(\"blink\");\n}\n";
const BROKEN: &str = "fn main() {\n    let _ = led;\n}\n";

struct Machine {
    config: TempDir,
    courses: TempDir,
}

impl Machine {
    /// A courses root with one fetched exercise whose `src/main.rs` is
    /// `main_rs` and whose ter.toml allows `modes`.
    fn with_exercise(main_rs: &str, modes: &[&str]) -> Self {
        let config = tempfile::tempdir().unwrap();
        let courses = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("config.toml"),
            format!("courses_root = {:?}\n", courses.path()),
        )
        .unwrap();
        let m = Self { config, courses };
        let dir = m.exercise();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"exercise\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), main_rs).unwrap();
        let sha = ter_sdk::project::source_sha256(&dir).unwrap();
        let modes: Vec<String> = modes.iter().map(|s| format!("{s:?}")).collect();
        std::fs::write(
            dir.join("ter.toml"),
            format!(
                "exercise = {EXERCISE:?}\ncourse = {COURSE:?}\ntarget = \"xiao-esp32c3-nostd\"\n\
                 modes = [{}]\nmode = {}\nfetched_sha256 = {sha:?}\n",
                modes.join(", "),
                modes[0]
            ),
        )
        .unwrap();
        m
    }

    fn exercise(&self) -> PathBuf {
        self.courses.path().join(COURSE).join(EXERCISE)
    }

    fn ter_in(&self, site: &str, args: &[&str], cwd: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ter"))
            .args(args)
            .current_dir(cwd)
            .env("TER_KEYCHAIN", "off")
            .env("TER_SITE_URL", site)
            .env("TER_CONFIG_DIR", self.config.path())
            .env("TER_TOKEN", "good-token")
            .output()
            .unwrap()
    }

    /// `ter <args> --json` in the exercise folder.
    fn json(&self, site: &str, args: &[&str]) -> (bool, Value) {
        self.json_in(site, args, &self.exercise())
    }

    fn json_in(&self, site: &str, args: &[&str], cwd: &Path) -> (bool, Value) {
        let mut all = args.to_vec();
        all.push("--json");
        let o = self.ter_in(site, &all, cwd);
        let v = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        });
        (o.status.success(), v)
    }
}

fn ok(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "message": v }))
}

fn site_error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(json!({"message": {"error": {"code": code, "message": message}}}))
}

async fn site(min_version: &str, run_answer: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.ping")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(json!({
            "ok": true, "user": USER, "server_time": "2026-09-27T12:00:00",
            "min_supported_version": min_version, "latest_version": "0.1.0",
            "download_url": null, "premium": true
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.run")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(run_answer)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.hint")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(json!({
            "number": 1, "text": "Where is `led` declared?", "exhausted": false
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .respond_with(ok(json!({"courses": [{
            "course_id": COURSE, "title": "ESP GPIO",
            "lessons": [{"lesson_id": "0111", "title": "Exercise: Heartbeat",
                "completed": false, "locked": false, "exercise": null,
                "exercises": [{"exercise_id": EXERCISE, "runner": "cargo",
                    "target": "xiao-esp32c3-nostd", "kind": "exercise"}]}]
        }]})))
        .mount(&server)
        .await;
    server
}

fn run_answer() -> ResponseTemplate {
    ok(json!({"name": "r-0001", "attempt": 3, "concept_deltas": [
        {"concept": "gpio-output", "label": "GPIO output", "before": 0.42,
         "after": 0.38, "correct": false, "weight": 0.5}
    ]}))
}

/// The bodies of the requests the site got for `function`.
async fn posted(server: &MockServer, function: &str) -> Vec<Value> {
    let wanted = format!("{API}.{function}");
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == wanted)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn a_build_failure_is_posted_with_the_compiler_tail() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(BROKEN, &["hardware", "simulation"]);

    let (ok, v) = m.json(&server.uri(), &["run", "--no-check"]);

    assert!(!ok, "a failed build exits non-zero");
    assert_eq!(v["error"]["code"], "build_failed", "{v}");
    assert_eq!(v["name"], "r-0001");
    assert_eq!(v["site"]["attempt"], 3);
    assert_eq!(v["site"]["concept_deltas"][0]["concept"], "gpio-output");

    let bodies = posted(&server, "run").await;
    assert_eq!(bodies.len(), 1);
    let p = &bodies[0]["payload"];
    assert_eq!(p["exercise_id"], EXERCISE);
    assert_eq!(p["target"], "xiao-esp32c3-nostd");
    assert_eq!(p["mode"], "hardware");
    assert_eq!(p["build_status"], "failed");
    assert_eq!(p["check_status"], "not_run");
    assert_eq!(p["cli_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(p["hints_used"], 0);
    assert!(p.get("elf_sha256").is_none(), "{p}");
    assert!(p.get("venue").is_none(), "nothing ran: {p}");
    let tail = p["compiler_tail"].as_str().unwrap();
    assert!(tail.contains("E0425"), "{tail}");
    assert!(!tail.contains('\u{1b}'), "no colour codes: {tail}");
    assert!(p["duration_ms"].as_u64().is_some());

    let dir = m.exercise();
    let log = std::fs::read_to_string(dir.join(".runs/1/build.log")).unwrap();
    assert!(log.contains("E0425"));
    assert!(dir.join(".ter/last-run.json").is_file());
}

#[tokio::test]
async fn a_build_only_run_posts_not_run_with_the_programs_hash() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);

    let (ok, v) = m.json(&server.uri(), &["run", "--no-check", "--sim"]);

    assert!(ok, "{v}");
    assert!(v.get("error").is_none());
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["mode"], "simulation");
    assert_eq!(p["build_status"], "passed");
    assert_eq!(p["check_status"], "not_run");
    assert!(p.get("compiler_tail").is_none());

    // Built into the course's shared cache, not the exercise's own target/.
    let elf = m.courses.path().join(COURSE).join(".target/debug/exercise");
    let expected = sha256_hex(&std::fs::read(&elf).unwrap());
    assert_eq!(p["elf_sha256"], expected);
    assert!(!m.exercise().join("target").exists());
    assert_eq!(v["recording"], json!(m.exercise().join(".runs/1")));
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test]
async fn a_mode_the_exercise_does_not_allow_is_refused_before_building() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);

    let (ok, v) = m.json(&server.uri(), &["run", "--no-check", "--sim"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "mode_not_allowed", "{v}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!m.exercise().join(".runs").exists());
}

#[tokio::test]
async fn an_outdated_ter_does_not_build_or_post() {
    let server = site("9.0.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);

    let (ok, v) = m.json(&server.uri(), &["run", "--no-check"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "outdated", "{v}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!m.exercise().join(".runs").exists());
}

#[tokio::test]
async fn running_on_a_venue_is_not_available_yet() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--no-check")
    );
    assert!(posted(&server, "run").await.is_empty());
}

#[tokio::test]
async fn a_refused_post_keeps_the_log_and_is_not_remembered() {
    let server = site(
        "0.1.0",
        site_error(429, "rate_limited", "One run per second. Try again."),
    )
    .await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);

    let (ok, v) = m.json(&server.uri(), &["run", "--no-check"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "rate_limited", "{v}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("One run per second."), "{message}");
    assert!(message.contains("build.log"), "{message}");
    assert!(m.exercise().join(".runs/1/build.log").is_file());
    assert!(!m.exercise().join(".ter/last-run.json").exists());
}

#[tokio::test]
async fn hints_taken_on_a_run_are_reported_by_the_next() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(BROKEN, &["hardware"]);
    let site = server.uri();

    let (ok, v) = m.json(&site, &["hint"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "no_run", "{v}");

    m.json(&site, &["run", "--no-check"]);
    let (ok, v) = m.json(&site, &["hint"]);
    assert!(ok, "{v}");
    assert_eq!(v["number"], 1);
    assert_eq!(v["text"], "Where is `led` declared?");
    assert_eq!(v["run"], "r-0001");
    assert_eq!(v["hints_used"], 1);

    let hint = &posted(&server, "hint").await[0];
    assert_eq!(hint["run"], "r-0001");
    let files: Vec<&str> = hint["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["Cargo.toml", "src/main.rs"]);
    assert_eq!(hint["files"][1]["content"], BROKEN);

    let (_, status) = m.json(&site, &["status"]);
    assert_eq!(status["last_run"]["hints_used"], 1, "{status}");

    // The site paces runs; the mock does not.
    m.json(&site, &["run", "--no-check"]);
    let runs = posted(&server, "run").await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["payload"]["hints_used"], 1);
    // A fresh run starts its own count.
    let (_, status) = m.json(&site, &["status"]);
    assert_eq!(status["last_run"]["hints_used"], 0, "{status}");
}

#[tokio::test]
async fn status_shows_the_last_run_here_and_every_exercise_elsewhere() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(BROKEN, &["hardware"]);
    let site = server.uri();

    let (ok, v) = m.json(&site, &["status"]);
    assert!(ok, "{v}");
    assert_eq!(v["exercise_id"], EXERCISE);
    assert_eq!(v["state"], "open");
    assert_eq!(v["last_run"], Value::Null);

    m.json(&site, &["run", "--no-check"]);
    let (ok, v) = m.json(&site, &["status"]);
    assert!(ok, "{v}");
    let last = &v["last_run"];
    assert_eq!(last["attempt"], 3);
    assert_eq!(last["run"], "r-0001");
    assert_eq!(last["build_status"], "failed");
    assert_eq!(last["concept_deltas"][0]["after"], 0.38);
    assert_eq!(v["edited"], false);

    let (ok, v) = m.json_in(&site, &["status"], m.courses.path());
    assert!(ok, "{v}");
    assert_eq!(v["user"], USER);
    let rows = v["exercises"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["last_run"]["attempt"], 3);

    let text = m.ter_in(&site, &["status"], &m.exercise());
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("attempt 3, build failed (hardware)"),
        "{text}"
    );
    assert!(text.contains("GPIO output"), "{text}");
}

#[tokio::test]
async fn status_disk_needs_no_account() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);
    m.json(&server.uri(), &["run", "--no-check"]);

    // No site at all: an unroutable URL.
    let (ok, v) = m.json_in(
        "http://127.0.0.1:9",
        &["status", "--disk"],
        m.courses.path(),
    );

    assert!(ok, "{v}");
    let course = &v["courses"][0];
    assert_eq!(course["course"], COURSE);
    assert!(course["shared_cache_bytes"].as_u64().unwrap() > 0);
    assert_eq!(course["exercises"][0]["exercise"], EXERCISE);
    assert!(course["exercises"][0]["build_bytes"].as_u64().unwrap() > 0);
    assert!(v["total_bytes"].as_u64().unwrap() >= course["total_bytes"].as_u64().unwrap());
}

#[tokio::test]
async fn text_run_says_what_was_posted() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(BROKEN, &["hardware"]);

    let o = m.ter_in(&server.uri(), &["run", "--no-check"], &m.exercise());

    assert!(!o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(out.contains("Build failed."), "{out}");
    assert!(out.contains("Posted attempt 3"), "{out}");
    assert!(out.contains("GPIO output"), "{out}");
    assert!(err.contains("E0425"), "cargo's errors are shown: {err}");
    assert!(err.contains("error[build_failed]"), "{err}");
}
