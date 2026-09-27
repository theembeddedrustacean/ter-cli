//! `ter ex` against a local mock of the site, on a temporary courses root.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API: &str = "/api/method/ter_courses.api";
const BLINKY: &str = "gpio-blinky--xiao-esp32c3-nostd";
const RUNNER: &str = "espflash flash --monitor --chip esp32c3";

/// A config directory whose `config.toml` puts courses in a temp folder.
struct Machine {
    config: TempDir,
    courses: TempDir,
}

impl Machine {
    fn new() -> Self {
        let config = tempfile::tempdir().unwrap();
        let courses = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("config.toml"),
            format!("courses_root = {:?}\n", courses.path()),
        )
        .unwrap();
        Self { config, courses }
    }

    fn ter(&self, site: &str, args: &[&str]) -> Output {
        self.ter_in(site, args, self.courses.path())
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

    fn json(&self, site: &str, args: &[&str]) -> (bool, Value) {
        let mut all = args.to_vec();
        all.push("--json");
        let o = self.ter(site, &all);
        let v = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&o.stdout)));
        (o.status.success(), v)
    }

    fn exercise_dir(&self, course: &str, id: &str) -> PathBuf {
        self.courses.path().join(course).join(id)
    }

    fn is_empty(&self) -> bool {
        std::fs::read_dir(self.courses.path()).unwrap().count() == 0
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn scaffold_files(config: Option<&str>) -> Value {
    let mut files = vec![
        json!({"path": "Cargo.toml", "content": "[package]\nname = \"exercise\"\n", "encoding": "utf8"}),
        json!({"path": "check.yaml", "content": "checks: []\n", "encoding": "utf8"}),
        json!({"path": "src/bin/main.rs", "content": "fn main() {}\n", "encoding": "utf8"}),
    ];
    if let Some(c) = config {
        files.push(json!({"path": ".cargo/config.toml", "content": c, "encoding": "utf8"}));
    }
    Value::Array(files)
}

fn exercise(id: &str, files: Value) -> Value {
    json!({
        "exercise_id": id, "title": "Heartbeat", "target": "xiao-esp32c3-nostd",
        "runner": "cargo", "kind": "exercise", "runner_args": {}, "timeout_seconds": 180,
        "toolchain": {"chip": "ESP32-C3"}, "instructions_md": "# Heartbeat", "files": files
    })
}

fn exercise_ref(id: &str) -> Value {
    json!({"exercise_id": id, "runner": "cargo", "target": "xiao-esp32c3-nostd",
           "kind": "exercise", "toolchain": {}})
}

fn site_error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(json!({"message": {"error": {"code": code, "message": message}}}))
}

async fn mock_site() -> MockServer {
    let server = MockServer::start().await;
    let ok = |v: Value| ResponseTemplate::new(200).set_body_json(json!({ "message": v }));
    Mock::given(method("GET"))
        .and(path(format!("{API}.enrollments")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(json!({"courses": [{
            "course_id": "esp-gpio", "title": "ESP GPIO",
            "lessons": [
                {"lesson_id": "0039 Overview", "title": "Overview", "completed": true,
                 "locked": false, "exercise": null, "exercises": []},
                {"lesson_id": "0111 Exercise", "title": "Exercise: Heartbeat", "completed": false,
                 "locked": false, "exercise": exercise_ref(BLINKY), "exercises": [exercise_ref(BLINKY)]},
                {"lesson_id": "0112 Exercise", "title": "Exercise: Toggle", "completed": false,
                 "locked": true, "exercise": exercise_ref("gpio-button--xiao-esp32c3-nostd"),
                 "exercises": [exercise_ref("gpio-button--xiao-esp32c3-nostd")]}
            ]}]})))
        .mount(&server)
        .await;
    let exercise_answer = |id: &str, answer: ResponseTemplate| {
        Mock::given(method("GET"))
            .and(path(format!("{API}.exercise")))
            .and(query_param("exercise_id", id))
            .respond_with(answer)
    };
    exercise_answer(
        BLINKY,
        ok(exercise(
            BLINKY,
            scaffold_files(Some(&format!(
                "[target.riscv32imc-unknown-none-elf]\nrunner = \"{RUNNER}\"\n"
            ))),
        )),
    )
    .mount(&server)
    .await;
    exercise_answer(
        "gpio-button--xiao-esp32c3-nostd",
        site_error(
            403,
            "locked",
            "Finish the previous exercise in this course first.",
        ),
    )
    .mount(&server)
    .await;
    exercise_answer(
        "std-blinky--xiao-esp32c3-std",
        site_error(403, "not_enrolled", "You are not enrolled in esp-std-gpio."),
    )
    .mount(&server)
    .await;
    exercise_answer(
        "no-runner--xiao-esp32c3-nostd",
        ok(exercise(
            "no-runner--xiao-esp32c3-nostd",
            scaffold_files(Some("[build]\ntarget = \"riscv32imc-unknown-none-elf\"\n")),
        )),
    )
    .mount(&server)
    .await;
    // A staff account may fetch from a course it is not enrolled in.
    exercise_answer(
        "staff-only--xiao-esp32c3-nostd",
        ok(exercise(
            "staff-only--xiao-esp32c3-nostd",
            scaffold_files(Some(&format!(
                "[target.riscv32imc-unknown-none-elf]\nrunner = \"{RUNNER}\"\n"
            ))),
        )),
    )
    .mount(&server)
    .await;
    // What the site sends once scaffolds carry a lock file and `exercise`
    // says which modes are allowed.
    let mut with_lock_and_modes = exercise(
        "sim-only--xiao-esp32c3-nostd",
        scaffold_files(Some(&format!(
            "[target.riscv32imc-unknown-none-elf]\nrunner = \"{RUNNER}\"\n"
        ))),
    );
    with_lock_and_modes["files"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path": "Cargo.lock", "content": "version = 4\n", "encoding": "utf8"}));
    with_lock_and_modes["modes"] = json!(["simulation"]);
    exercise_answer("sim-only--xiao-esp32c3-nostd", ok(with_lock_and_modes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{API}.exercise")))
        .and(header("authorization", "Bearer revoked-token"))
        .respond_with(site_error(401, "token_invalid", "Token revoked."))
        .with_priority(1)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn list_shows_courses_exercises_and_state() {
    let site = mock_site().await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "list"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("ESP GPIO (esp-gpio)"), "{out}");
    assert!(
        out.contains(&format!("open    {BLINKY}  Exercise: Heartbeat")),
        "{out}"
    );
    assert!(
        out.contains("locked  gpio-button--xiao-esp32c3-nostd"),
        "{out}"
    );
    assert!(!out.contains("Overview"), "lessons with no exercise: {out}");
    assert!(!out.contains("fetched"), "{out}");

    assert!(
        m.ter(&site.uri(), &["ex", "fetch", BLINKY])
            .status
            .success()
    );
    let out = stdout(&m.ter(&site.uri(), &["ex", "list"]));
    assert!(out.contains("Exercise: Heartbeat  fetched"), "{out}");
}

#[tokio::test]
async fn list_json_and_course_filter() {
    let site = mock_site().await;
    let m = Machine::new();
    let (ok, v) = m.json(&site.uri(), &["ex", "list", "--course", "esp-gpio"]);
    assert!(ok, "{v}");
    let ex = &v["courses"][0]["exercises"];
    assert_eq!(ex.as_array().unwrap().len(), 2, "{v}");
    assert_eq!(ex[0]["exercise_id"], BLINKY);
    assert_eq!(ex[0]["locked"], false);
    assert_eq!(ex[0]["fetched"], Value::Null);
    assert_eq!(ex[1]["locked"], true);

    let (ok, v) = m.json(&site.uri(), &["ex", "list", "--course", "esp-std"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_enrolled", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Your courses: esp-gpio")
    );
}

#[tokio::test]
async fn fetch_writes_the_scaffold_and_ter_toml_in_the_course_folder() {
    let site = mock_site().await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "fetch", BLINKY]);
    assert!(o.status.success(), "{}", stderr(&o));
    let dir = m.exercise_dir("esp-gpio", BLINKY);
    assert!(
        stdout(&o).contains(&dir.display().to_string()),
        "{}",
        stdout(&o)
    );
    assert!(stdout(&o).contains(RUNNER), "{}", stdout(&o));

    for f in [
        "Cargo.toml",
        "check.yaml",
        "src/bin/main.rs",
        ".cargo/config.toml",
    ] {
        assert!(dir.join(f).is_file(), "{f}");
    }
    let ter: toml::Table =
        toml::from_str(&std::fs::read_to_string(dir.join("ter.toml")).unwrap()).unwrap();
    assert_eq!(ter["exercise"].as_str(), Some(BLINKY));
    assert_eq!(ter["course"].as_str(), Some("esp-gpio"));
    assert_eq!(ter["target"].as_str(), Some("xiao-esp32c3-nostd"));
    assert_eq!(ter["mode"].as_str(), Some("hardware"));
    assert_eq!(
        ter["modes"].as_array().unwrap(),
        &[
            toml::Value::from("hardware"),
            toml::Value::from("simulation")
        ]
    );
    assert_eq!(ter["fetched_sha256"].as_str().unwrap().len(), 64);

    let (ok, v) = m.json(&site.uri(), &["ex", "fetch", BLINKY]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "already_fetched", "{v}");
}

#[tokio::test]
async fn fetch_json_and_explicit_dir() {
    let site = mock_site().await;
    let m = Machine::new();
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("mine");
    let (ok, v) = m.json(
        &site.uri(),
        &["ex", "fetch", BLINKY, dest.to_str().unwrap()],
    );
    assert!(ok, "{v}");
    assert_eq!(v["path"], dest.to_str().unwrap());
    assert_eq!(v["course"], "esp-gpio");
    assert_eq!(v["runner"], RUNNER);
    assert_eq!(v["dev"], false);
    assert!(dest.join("ter.toml").is_file());
    assert!(m.is_empty(), "nothing under the courses root");
}

#[tokio::test]
async fn fetch_passes_the_sites_refusals_through_and_writes_nothing() {
    let site = mock_site().await;
    let m = Machine::new();
    for (id, code) in [
        ("gpio-button--xiao-esp32c3-nostd", "locked"),
        ("std-blinky--xiao-esp32c3-std", "not_enrolled"),
    ] {
        let o = m.ter(&site.uri(), &["ex", "fetch", id]);
        assert!(!o.status.success());
        assert!(
            stderr(&o).starts_with(&format!("error[{code}]")),
            "{}",
            stderr(&o)
        );
    }
    assert!(m.is_empty());
}

#[tokio::test]
async fn fetch_refuses_a_scaffold_with_no_runner() {
    let site = mock_site().await;
    let m = Machine::new();
    let (ok, v) = m.json(
        &site.uri(),
        &["ex", "fetch", "no-runner--xiao-esp32c3-nostd"],
    );
    assert!(!ok);
    assert_eq!(v["error"]["code"], "bad_scaffold", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no runner")
    );
    assert!(m.is_empty(), "nothing is written");
}

#[tokio::test]
async fn fetch_outside_the_enrolled_courses_goes_to_unlisted() {
    let site = mock_site().await;
    let m = Machine::new();
    let (ok, v) = m.json(
        &site.uri(),
        &["ex", "fetch", "staff-only--xiao-esp32c3-nostd"],
    );
    assert!(ok, "{v}");
    assert_eq!(v["course"], "unlisted");
    assert!(
        m.exercise_dir("unlisted", "staff-only--xiao-esp32c3-nostd")
            .join("ter.toml")
            .is_file()
    );
}

#[tokio::test]
async fn fetch_with_a_revoked_stored_token_forgets_it() {
    let site = mock_site().await;
    let m = Machine::new();
    let token_file = m.config.path().join("token");
    std::fs::write(&token_file, "revoked-token").unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["ex", "fetch", BLINKY])
        .env("TER_KEYCHAIN", "off")
        .env("TER_SITE_URL", site.uri())
        .env("TER_CONFIG_DIR", m.config.path())
        .env_remove("TER_TOKEN")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("Token revoked."), "{}", stderr(&o));
    assert!(!token_file.exists());
}

#[tokio::test]
async fn fetch_uses_the_sites_modes_and_writes_its_lock_file() {
    let site = mock_site().await;
    let m = Machine::new();
    let id = "sim-only--xiao-esp32c3-nostd";
    let (ok, v) = m.json(&site.uri(), &["ex", "fetch", id]);
    assert!(ok, "{v}");
    assert_eq!(v["modes"], json!(["simulation"]));
    assert_eq!(v["mode"], "simulation");
    let dir = m.exercise_dir("unlisted", id);
    assert_eq!(
        std::fs::read_to_string(dir.join("Cargo.lock")).unwrap(),
        "version = 4\n"
    );

    // Cargo may rewrite the lock file; that is not an edit.
    std::fs::write(dir.join("Cargo.lock"), "version = 4\n# updated by cargo\n").unwrap();
    let (ok, v) = m.json(&site.uri(), &["ex", "remove", id]);
    assert!(ok, "{v}");
    assert_eq!(v["edited"], false);
}

#[tokio::test]
async fn clean_drops_build_output_and_keeps_source() {
    let site = mock_site().await;
    let m = Machine::new();
    assert!(
        m.ter(&site.uri(), &["ex", "fetch", BLINKY])
            .status
            .success()
    );
    let dir = m.exercise_dir("esp-gpio", BLINKY);
    let course = m.courses.path().join("esp-gpio");
    let fill = |d: PathBuf| {
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("blob"), vec![0u8; 2048]).unwrap();
    };
    fill(dir.join("target/debug"));
    fill(dir.join(".runs/1"));
    fill(course.join(".target/riscv32imc-unknown-none-elf"));

    // In the exercise folder, with no argument.
    let o = m.ter_in(&site.uri(), &["ex", "clean"], &dir.join("src"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("Freed 4.0 KB."), "{}", stdout(&o));
    assert!(stdout(&o).contains("ter ex clean --course esp-gpio"));
    assert!(!dir.join("target").exists() && !dir.join(".runs").exists());
    assert!(dir.join("src/bin/main.rs").is_file());
    assert!(course.join(".target").exists());

    let (ok, v) = m.json(&site.uri(), &["ex", "clean", "--course", "esp-gpio"]);
    assert!(ok, "{v}");
    assert_eq!(v["freed_bytes"], 2048);
    assert!(!course.join(".target").exists());

    fill(course.join(".embuild"));
    let (ok, v) = m.json(&site.uri(), &["ex", "clean", "--all"]);
    assert!(ok, "{v}");
    assert_eq!(v["removed_dirs"], 1);
    assert!(dir.join("ter.toml").is_file());

    let o = m.ter(&site.uri(), &["ex", "clean", BLINKY]);
    assert!(stdout(&o).contains("Nothing to clean."), "{}", stdout(&o));
}

#[tokio::test]
async fn clean_outside_an_exercise_says_what_to_name() {
    let site = mock_site().await;
    let m = Machine::new();
    let (ok, v) = m.json(&site.uri(), &["ex", "clean"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_an_exercise", "{v}");
    let (ok, v) = m.json(&site.uri(), &["ex", "clean", "nope"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_fetched", "{v}");
}

#[tokio::test]
async fn remove_refuses_edits_unless_forced() {
    let site = mock_site().await;
    let m = Machine::new();
    assert!(
        m.ter(&site.uri(), &["ex", "fetch", BLINKY])
            .status
            .success()
    );
    let dir = m.exercise_dir("esp-gpio", BLINKY);
    std::fs::write(dir.join("src/bin/main.rs"), "fn main() { loop {} }\n").unwrap();

    let (ok, v) = m.json(&site.uri(), &["ex", "remove", BLINKY]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "source_edited", "{v}");
    assert!(dir.exists());

    let (ok, v) = m.json(&site.uri(), &["ex", "remove", BLINKY, "--force"]);
    assert!(ok, "{v}");
    assert_eq!(v["edited"], true);
    assert!(!dir.exists());
}

#[tokio::test]
async fn remove_an_unedited_exercise_and_fetch_it_again() {
    let site = mock_site().await;
    let m = Machine::new();
    assert!(
        m.ter(&site.uri(), &["ex", "fetch", BLINKY])
            .status
            .success()
    );
    let dir = m.exercise_dir("esp-gpio", BLINKY);
    std::fs::write(dir.join("Cargo.lock"), "# written by cargo").unwrap();
    let o = m.ter(&site.uri(), &["ex", "remove", BLINKY]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!dir.exists());
    assert!(
        m.ter(&site.uri(), &["ex", "fetch", BLINKY])
            .status
            .success()
    );
}

/// A stand-in curriculum checkout: `tools/exercise_sync.py --print` gives
/// one payload per target, and the sync state names the site's course.
fn fake_curriculum() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tools")).unwrap();
    std::fs::create_dir_all(dir.path().join(".ter-sync")).unwrap();
    let payload = |id: &str| {
        let mut p = exercise(
            id,
            scaffold_files(Some(&format!("[target.x]\nrunner = \"{RUNNER}\"\n"))),
        );
        p["lesson"] = json!("0111 Exercise: Heartbeat");
        p["allow_hardware"] = json!(0);
        p["allow_simulation"] = json!(1);
        p["runner_args"] = json!("{}");
        p["toolchain"] = json!("{}");
        p
    };
    let payloads = json!([payload(BLINKY), payload("gpio-blinky--xiao-esp32c6-nostd")]);
    std::fs::write(
        dir.path().join("tools/exercise_sync.py"),
        format!(
            "import sys\nassert sys.argv[1:] == ['--exercise', 'gpio-blinky', '--print'], sys.argv\nprint({:?})\n",
            payloads.to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".ter-sync/state.json"),
        json!({
            "esp-nostd-gpio": "esp-bare-metal-gpio-programming",
            "esp-nostd-gpio/02-blinky/exercise-gpio-blinky": "0111 Exercise: Heartbeat"
        })
        .to_string(),
    )
    .unwrap();
    dir
}

#[test]
fn fetch_dev_reads_a_curriculum_checkout_without_the_site() {
    let curriculum = fake_curriculum();
    let m = Machine::new();
    let site = "http://127.0.0.1:9";
    let (ok, v) = m.json(
        site,
        &[
            "ex",
            "fetch",
            BLINKY,
            "--dev",
            curriculum.path().to_str().unwrap(),
        ],
    );
    assert!(ok, "{v}");
    assert_eq!(v["dev"], true);
    assert_eq!(v["course"], "esp-bare-metal-gpio-programming");
    assert_eq!(v["modes"], json!(["simulation"]));
    assert_eq!(v["mode"], "simulation");
    let dir = m.exercise_dir("esp-bare-metal-gpio-programming", BLINKY);
    assert!(dir.join("src/bin/main.rs").is_file());

    let (ok, v) = m.json(
        site,
        &[
            "ex",
            "fetch",
            "gpio-blinky--nrf52",
            "--dev",
            curriculum.path().to_str().unwrap(),
        ],
    );
    assert!(!ok);
    assert_eq!(v["error"]["code"], "dev_error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("gpio-blinky--xiao-esp32c6-nostd"),
        "{v}"
    );
}
