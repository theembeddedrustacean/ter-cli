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

/// gpio-blinky's check, plus the banner the generated project prints.
const HEARTBEAT_CHECK: &str = "\
timeout_ms: 5500
assert:
  - id: banner
    serial_contains: \"Hello world!\"
    within_ms: 2000
  - id: heartbeat-rate
    pin: user_led
    toggles_per_s: { min: 3.6, max: 4.4 }
    window_ms: [900, 4900]
  - id: flash-width
    pin: user_led
    pulse_width_ms: { min: 80, max: 120 }
    window_ms: [900, 4900]
";

/// The course's uFerris circuit, trimmed to the LED and the button.
const UFERRIS: &str = r#"{
  "version": 1,
  "parts": [
    {"type": "board-xiao-esp32-c3", "id": "xiao", "top": 60, "left": 0, "attrs": {}},
    {"type": "wokwi-led", "id": "led1", "top": -30, "left": 140, "attrs": {"color": "red"}},
    {"type": "wokwi-resistor", "id": "r1", "top": 30, "left": 150, "attrs": {"value": "220"}},
    {"type": "wokwi-pushbutton", "id": "btn1", "top": 120, "left": 150, "attrs": {"bounce": "0"}}
  ],
  "connections": [
    ["xiao:D1", "r1:1", "green", []],
    ["r1:2", "led1:A", "green", []],
    ["led1:C", "xiao:GND", "black", []],
    ["btn1:1.l", "xiao:D3", "orange", []],
    ["btn1:2.l", "xiao:GND", "black", []]
  ]
}
"#;

/// Plays `wokwi-cli`'s part: prints the scenario's step markers around the
/// program's serial output, writes the serial log and the VCD, and exits
/// 42, its code for a run that reached `--timeout`. It keeps what it was
/// given, for the test to look at.
const FAKE_WOKWI_CLI: &str = r#"#!/bin/sh
dir="$1"; shift
while [ $# -gt 0 ]; do
  case "$1" in
    --serial-log-file) serial="$2"; shift ;;
    --vcd-file) vcd="$2"; shift ;;
    --scenario) scenario="$2"; shift ;;
    --timeout) timeout="$2"; shift ;;
  esac
  shift
done
here="$(dirname "$0")"
[ "$WOKWI_CLI_TOKEN" = "fake-wokwi" ] || { echo "Error: bad token" >&2; exit 1; }
cp "$dir/diagram.json" "$here/got-diagram.json"
cp "$scenario" "$here/got-scenario.yaml"
echo "$timeout" > "$here/got-timeout"
code="$(cat "$here/exit-code")"
if [ "$code" != 42 ]; then
  echo "API Error: You have used up your monthly simulation minutes" >&2
  exit "$code"
fi
printf '[ter-clock] Executing step: @0
[ter-clock] delay 50ms
'
printf 'Hello world!
'
printf '[ter-clock] Executing step: @50
[ter-clock] delay 50ms
'
printf '[ter-clock] Executing step: @100
[ter-clock] delay 50ms
'
printf 'Hello world!
' > "$serial"
[ -z "$vcd" ] || cp "$here/fixture.vcd" "$vcd"
echo "Timeout: simulation did not finish in ${timeout}ms" >&2
exit 42
"#;

const GOOD: &str = "fn main() {\n    println!(\"blink\");\n}\n";
const BROKEN: &str = "fn main() {\n    let _ = led;\n}\n";

struct Machine {
    config: TempDir,
    courses: TempDir,
    /// A stand-in for `wokwi-cli`, first on the PATH, when set.
    fake_wokwi: Option<TempDir>,
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
        let m = Self {
            config,
            courses,
            fake_wokwi: None,
        };
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ter"));
        cmd.args(args)
            .current_dir(cwd)
            .env("TER_KEYCHAIN", "off")
            .env("TER_SITE_URL", site)
            .env("TER_CONFIG_DIR", self.config.path())
            .env("TER_TOKEN", "good-token")
            .env_remove("WOKWI_CLI_TOKEN");
        if let Some(fake) = &self.fake_wokwi {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs = vec![fake.path().to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(dirs).unwrap())
                .env("WOKWI_CLI_TOKEN", "fake-wokwi");
        }
        cmd.output().unwrap()
    }

    /// Make the exercise a simulated one: `check` as its check.yaml, the
    /// uFerris circuit, the target's pins in ter.toml, and a fake
    /// `wokwi-cli` that plays back `vcd` (a file in tests/fixtures/wokwi)
    /// and exits with `exit_code`.
    #[cfg(unix)]
    fn simulated(mut self, check: &str, vcd: &str, exit_code: i32) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.exercise();
        std::fs::write(dir.join("check.yaml"), check).unwrap();
        std::fs::write(dir.join("diagram.json"), UFERRIS).unwrap();
        let mut ter = std::fs::read_to_string(dir.join("ter.toml")).unwrap();
        ter.push_str("\n[pins]\nuser_led = \"GPIO3\"\nuser_button = \"GPIO5\"\n");
        std::fs::write(dir.join("ter.toml"), ter).unwrap();

        let fake = tempfile::tempdir().unwrap();
        let script = fake.path().join("wokwi-cli");
        std::fs::write(&script, FAKE_WOKWI_CLI).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/wokwi")
            .join(vcd);
        std::fs::copy(fixture, fake.path().join("fixture.vcd")).unwrap();
        std::fs::write(fake.path().join("exit-code"), exit_code.to_string()).unwrap();
        self.fake_wokwi = Some(fake);
        self
    }

    fn fake_saw(&self, file: &str) -> String {
        let fake = self.fake_wokwi.as_ref().unwrap();
        std::fs::read_to_string(fake.path().join(file)).unwrap()
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
async fn a_checked_run_on_a_board_is_not_available_yet() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);
    std::fs::write(m.exercise().join("check.yaml"), HEARTBEAT_CHECK).unwrap();

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("--sim"));
    assert!(posted(&server, "run").await.is_empty());
    assert!(
        !m.exercise().join(".runs").exists(),
        "refused before the build"
    );
}

#[tokio::test]
async fn no_check_yaml_builds_only_in_either_mode() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);

    for mode in ["--sim", "--hw"] {
        let (ok, v) = m.json(&server.uri(), &["run", mode]);
        assert!(ok, "{v}");
    }
    let bodies = posted(&server, "run").await;
    assert_eq!(bodies.len(), 2);
    for (b, mode) in bodies.iter().zip(["simulation", "hardware"]) {
        let p = &b["payload"];
        assert_eq!(p["mode"], mode);
        assert_eq!(p["build_status"], "passed");
        assert_eq!(p["check_status"], "not_run");
        assert_eq!(
            (&p["checks_seen"], &p["checks_total"]),
            (&json!(0), &json!(0))
        );
        assert!(p.get("venue").is_none(), "nothing ran: {p}");
    }
    let text = m.ter_in(&server.uri(), &["run", "--sim"], &m.exercise());
    assert!(
        String::from_utf8_lossy(&text.stdout)
            .contains("This exercise has no automatic check yet. Run it with cargo run."),
        "{}",
        String::from_utf8_lossy(&text.stdout)
    );
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

#[cfg(unix)]
#[tokio::test]
async fn a_simulated_run_is_recorded_judged_and_posted() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let learners_diagram = std::fs::read_to_string(m.exercise().join("diagram.json")).unwrap();

    let (ok, v) = m.json(&server.uri(), &["run", "--sim"]);

    assert!(ok, "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["mode"], "simulation");
    assert_eq!(p["venue"], "wokwi");
    assert_eq!(p["build_status"], "passed");
    assert_eq!(p["check_status"], "passed", "{v}");
    assert_eq!(
        (&p["checks_seen"], &p["checks_total"]),
        (&json!(3), &json!(3))
    );
    assert!(p.get("first_failure").is_none());
    assert_eq!(p["transcript_tail"], "Hello world!\n");
    assert_eq!(
        v["verdicts"][1]["observed"],
        "4 toggles per second (16 level changes)"
    );
    assert_eq!(v["verdicts"][2]["observed"], "8 pulses, 100 ms");

    // The run copy of the circuit got the analyzer on the LED; the
    // learner's did not.
    let run = m.exercise().join(".runs/1");
    let got: Value = serde_json::from_str(&m.fake_saw("got-diagram.json")).unwrap();
    let conns = got["connections"].as_array().unwrap();
    assert!(
        conns.contains(&json!(["xiao:D1", "ter_logic:D0", "violet", []])),
        "{got}"
    );
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("diagram.json")).unwrap(),
        learners_diagram
    );
    assert_eq!(m.fake_saw("got-timeout").trim(), "5500");
    for f in [
        "diagram.json",
        "scenario.yaml",
        "events.jsonl",
        "check.toml",
        "check.yaml",
        "wokwi.vcd",
        "build.log",
    ] {
        assert!(run.join(f).is_file(), "{f} is in the run folder");
    }

    // Re-checking the recording offline gives check.toml back byte for byte.
    let o = m.ter_in(
        "http://127.0.0.1:9",
        &["telemetry", "check", ".runs/1", "--check", "check.yaml"],
        &m.exercise(),
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.stdout, std::fs::read(run.join("check.toml")).unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn a_wrong_program_fails_the_right_check() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "even-blink.vcd",
        42,
    );

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "check_failed", "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["check_status"], "failed");
    let ff: Value = serde_json::from_str(p["first_failure"].as_str().unwrap()).unwrap();
    assert_eq!(
        ff,
        json!({
            "id": "heartbeat-rate",
            "expected": "3.6 to 4.4 toggles per second between 900 and 4900 ms",
            "observed": "2 toggles per second (8 level changes)"
        })
    );

    // Offline, the same verdict, with a hint, and the same exit code.
    let o = m.ter_in(
        "http://127.0.0.1:9",
        &["telemetry", "check", ".runs/1", "--check", "check.yaml"],
        &m.exercise(),
    );
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("hint: heartbeat-rate: expected"), "{err}");
    assert!(err.contains("error[check_failed]"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_wokwi_quota_failure_is_posted_as_not_run() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        1,
    );

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["check_status"], "not_run", "never failed");
    assert!(p.get("first_failure").is_none());
    let tail = p["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"));
    assert!(tail.contains("monthly simulation minutes"), "{tail}");
}

#[cfg(unix)]
#[tokio::test]
async fn no_wokwi_token_stops_before_the_build() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    // Keep the circuit, lose the fake (and with it the token).
    m.fake_wokwi = None;

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!m.exercise().join(".runs").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_press_is_scheduled_on_the_button_wired_to_the_pin() {
    let server = site("0.1.0", run_answer()).await;
    let check = "timeout_ms: 300\nsetup:\n  - press: user_button\n    at_ms: 120\n    hold_ms: 30\nassert:\n  - id: banner\n    serial_contains: \"Hello\"\n    within_ms: 200\n";
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(check, "heartbeat.vcd", 42);

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(ok, "{v}");
    let scenario = m.fake_saw("got-scenario.yaml");
    assert!(
        scenario.contains("  - set-control:\n      part-id: btn1\n      control: pressed\n      value: 1\n  - name: \"@120\""),
        "{scenario}"
    );
    assert!(
        scenario.contains("value: 0\n  - name: \"@150\""),
        "{scenario}"
    );
    let got: Value = serde_json::from_str(&m.fake_saw("got-diagram.json")).unwrap();
    assert!(
        !got["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "wokwi-logic-analyzer"),
        "no pin checks, no analyzer"
    );
}
