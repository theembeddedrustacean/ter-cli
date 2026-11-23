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
async fn fetch_writes_a_gitignore_and_the_exercise_is_still_unedited() {
    let site = mock_site().await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "fetch", BLINKY]);
    assert!(o.status.success(), "{}", stderr(&o));
    let dir = m.exercise_dir("esp-gpio", BLINKY);
    let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    for line in [".ter/", ".runs/", "target/"] {
        assert!(ignore.lines().any(|l| l == line), "{line} in {ignore}");
    }
    let o = m.ter(&site.uri(), &["ex", "remove", BLINKY]);
    assert!(o.status.success(), "{}", stderr(&o));
}

#[tokio::test]
async fn the_courses_root_comes_from_the_flag_then_the_environment_then_the_config() {
    let site = mock_site().await;
    let m = Machine::new();
    let from_env = tempfile::tempdir().unwrap();
    let from_flag = tempfile::tempdir().unwrap();
    let ter = |args: &[&str], env_root: Option<&Path>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ter"));
        c.args(args)
            .current_dir(m.courses.path())
            .env("TER_KEYCHAIN", "off")
            .env("TER_SITE_URL", site.uri())
            .env("TER_CONFIG_DIR", m.config.path())
            .env("TER_TOKEN", "good-token");
        match env_root {
            Some(root) => c.env("TER_COURSES_ROOT", root),
            None => c.env_remove("TER_COURSES_ROOT"),
        };
        c.output().unwrap()
    };
    let fetched = |root: &Path| {
        root.join("esp-gpio")
            .join(BLINKY)
            .join("ter.toml")
            .is_file()
    };

    // The environment beats config.toml.
    let o = ter(&["ex", "fetch", BLINKY], Some(from_env.path()));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(fetched(from_env.path()));
    assert!(m.is_empty(), "nothing under the config's root");

    // The flag beats the environment, before or after the subcommand.
    let flag = from_flag.path().to_str().unwrap();
    let o = ter(
        &["ex", "fetch", BLINKY, "--root", flag],
        Some(from_env.path()),
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(fetched(from_flag.path()));

    // Other commands look under the same root.
    let o = ter(&["--root", flag, "ex", "remove", BLINKY], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!fetched(from_flag.path()));
    assert!(fetched(from_env.path()), "the other root is untouched");

    // An empty variable is no root at all: the config's applies.
    let o = ter(&["ex", "fetch", BLINKY], Some(Path::new("")));
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(fetched(m.courses.path()));

    // A relative --root is taken from the current folder.
    let o = ter(&["ex", "remove", BLINKY], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = ter(&["ex", "fetch", BLINKY, "--root", "mine"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(fetched(&m.courses.path().join("mine")));
}

#[test]
fn fetch_help_says_how_to_choose_the_folder() {
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["ex", "fetch", "--help"])
        .output()
        .unwrap();
    let help = stdout(&o);
    for needle in ["--root", "TER_COURSES_ROOT", "courses_root", "<id> <dir>"] {
        assert!(help.contains(needle), "{needle} in {help}");
    }
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
    let target = dir.path().join("targets/xiao-esp32c3-nostd");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(
        target.join("target.yaml"),
        "id: xiao-esp32c3-nostd\npins:\n  user_led: GPIO3\n  qwiic_i2c: { sda: GPIO6, scl: GPIO7 }\n",
    )
    .unwrap();
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
    let ter = std::fs::read_to_string(dir.join("ter.toml")).unwrap();
    assert!(
        ter.contains("[pins]\nuser_led = \"GPIO3\""),
        "the target's pin map is written: {ter}"
    );

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

const SERIAL_CHECK: &str = "timeout_ms: 2000\nassert:\n  - id: banner\n    serial_contains: \"ready\"\n    within_ms: 2000\n";
const PIN_CHECK: &str = "timeout_ms: 3000\nassert:\n  - id: banner\n    serial_contains: \"ready\"\n    within_ms: 2000\n  - id: blink-rate\n    pin: user_led\n    toggles_per_s: { min: 1.8, max: 2.2 }\n    window_ms: [600, 2600]\n";
const SETUP_CHECK: &str = "timeout_ms: 3000\nsetup:\n  - press: user_button\n    at_ms: 500\nassert:\n  - id: pressed\n    serial_contains: \"pressed\"\n    within_ms: 2000\n";

/// The mock site, plus `id` answering with `check` (or no check.yaml) and
/// `modes`.
async fn site_with(id: &str, check: Option<&str>, modes: &[&str]) -> MockServer {
    // An exercise that allows simulation comes with its circuit.
    site_with_circuit(id, check, modes, modes.contains(&"simulation")).await
}

async fn site_with_circuit(
    id: &str,
    check: Option<&str>,
    modes: &[&str],
    circuit: bool,
) -> MockServer {
    site_with_files(id, check, modes, circuit, &[]).await
}

/// The same, with `extra` files (path, content) in the exercise.
async fn site_with_files(
    id: &str,
    check: Option<&str>,
    modes: &[&str],
    circuit: bool,
    extra: &[(&str, &str)],
) -> MockServer {
    let site = mock_site().await;
    let mut files = vec![
        json!({"path": "Cargo.toml", "content": "[package]\nname = \"exercise\"\n", "encoding": "utf8"}),
        json!({"path": "src/bin/main.rs", "content": "fn main() {}\n", "encoding": "utf8"}),
        json!({"path": ".cargo/config.toml", "content": format!("[target.riscv32imc-unknown-none-elf]\nrunner = \"{RUNNER}\"\n"), "encoding": "utf8"}),
    ];
    if let Some(c) = check {
        files.push(json!({"path": "check.yaml", "content": c, "encoding": "utf8"}));
    }
    if circuit {
        files.push(
            json!({"path": "diagram.json", "content": "{\"parts\":[]}\n", "encoding": "utf8"}),
        );
    }
    for (path, content) in extra {
        files.push(json!({"path": path, "content": content, "encoding": "utf8"}));
    }
    let mut ex = exercise(id, Value::Array(files));
    ex["modes"] = json!(modes);
    Mock::given(method("GET"))
        .and(path(format!("{API}.exercise")))
        .and(query_param("exercise_id", id))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": ex })))
        .mount(&site)
        .await;
    site
}

#[tokio::test]
async fn show_by_id_asks_the_site_once_and_writes_nothing() {
    let id = "serial-only--xiao-esp32c3-nostd";
    let site = site_with(id, Some(SERIAL_CHECK), &["hardware", "simulation"]).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "show", id]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}\n{}", stderr(&o));
    assert!(out.starts_with(&format!("Heartbeat ({id})\n")), "{out}");
    assert!(out.contains("checks  from the site (not fetched)"), "{out}");
    assert!(
        out.contains("banner  serial_contains  graded on a board and in simulation"),
        "{out}"
    );
    assert!(
        out.ends_with(
            "Passes on your board (`ter run --hw`) or in simulation (`ter run --sim`).\n"
        ),
        "{out}"
    );
    let (ok, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert!(ok, "{v}");
    assert_eq!(v["source"], "site");
    assert_eq!(v["passes_in"], json!(["hardware", "simulation"]));
    assert_eq!(v["checks"][0]["board_grades"], true);
    assert!(m.is_empty(), "show writes nothing");
    let asked = site
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with(".exercise"))
        .count();
    assert_eq!(asked, 2, "one exercise call per show");
}

#[tokio::test]
async fn show_says_a_pin_check_is_graded_only_in_simulation() {
    let id = "pins--xiao-esp32c3-nostd";
    let site = site_with(id, Some(PIN_CHECK), &["hardware", "simulation"]).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "show", id]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}\n{}", stderr(&o));
    assert!(
        out.contains("banner      serial_contains  graded on a board and in simulation"),
        "{out}"
    );
    assert!(
        out.contains(
            "blink-rate  toggles_per_s    graded only in simulation; a board shows it, but ter can't grade it there"
        ),
        "{out}"
    );
    assert!(
        out.ends_with(
            "Passes only in simulation: `ter run --sim`. A run on a board can't complete it.\n"
        ),
        "{out}"
    );
    let (_, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert_eq!(v["passes_in"], json!(["simulation"]));
}

#[tokio::test]
async fn show_says_a_setup_needs_the_simulator_even_for_a_serial_check() {
    let id = "button--xiao-esp32c3-nostd";
    let site = site_with(id, Some(SETUP_CHECK), &["hardware", "simulation"]).await;
    let m = Machine::new();
    let (ok, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert!(ok, "{v}");
    assert_eq!(v["checks"][0]["board_grades"], false);
    assert!(
        v["checks"][0]["graded"]
            .as_str()
            .unwrap()
            .contains("only the simulator drives"),
        "{v}"
    );
    assert_eq!(v["passes_in"], json!(["simulation"]));
}

#[tokio::test]
async fn show_a_hardware_only_pin_check_points_at_the_lesson_page() {
    let id = "hw-pins--xiao-esp32c3-nostd";
    let site = site_with(id, Some(PIN_CHECK), &["hardware"]).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "show", id]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("graded only with a telemetry board, which ter does not support yet"),
        "{out}"
    );
    assert!(out.contains("No ter run can pass it yet"), "{out}");
    assert!(out.contains("lesson page"), "{out}");
    assert!(!out.contains("--sim"), "{out}");
}

#[tokio::test]
async fn show_reads_a_fetched_exercise_offline_from_its_own_check_yaml() {
    let id = "serial-only--xiao-esp32c3-nostd";
    let site = site_with(id, Some(SERIAL_CHECK), &["hardware", "simulation"]).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "fetch", id]);
    assert!(o.status.success(), "{}", stderr(&o));
    let dir = m.exercise_dir("unlisted", id);
    // The fetched copy is what show reads, not the site's.
    std::fs::write(dir.join("check.yaml"), PIN_CHECK).unwrap();
    let offline = "http://127.0.0.1:9";
    for (args, cwd) in [
        (vec!["ex", "show"], dir.clone()),
        (vec!["ex", "show", id], m.courses.path().to_path_buf()),
    ] {
        let o = m.ter_in(offline, &args, &cwd);
        let out = stdout(&o);
        assert!(o.status.success(), "{args:?}: {out}\n{}", stderr(&o));
        assert!(
            out.contains(&format!(
                "checks  from {} (the copy fetched; the site's can be newer)",
                dir.join("check.yaml").display()
            )),
            "{out}"
        );
        assert!(out.contains(&format!("folder  {}", dir.display())), "{out}");
        assert!(out.contains("blink-rate"), "{out}");
        assert!(out.contains("Passes only in simulation"), "{out}");
    }
}

/// Modes that allow simulation, but no circuit came with the exercise:
/// show does not offer the simulation run, by id or in the fetched folder.
#[tokio::test]
async fn show_does_not_offer_simulation_without_a_circuit() {
    let id = "serial-only--xiao-esp32c3-nostd";
    let site = site_with_circuit(id, Some(SERIAL_CHECK), &["hardware", "simulation"], false).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "fetch", id]);
    assert!(o.status.success(), "{}", stderr(&o));
    let dir = m.exercise_dir("unlisted", id);
    for (uri, args, cwd, source) in [
        (
            site.uri(),
            vec!["ex", "show", id],
            m.config.path().to_path_buf(),
            "folder",
        ),
        (
            "http://127.0.0.1:9".to_string(),
            vec!["ex", "show"],
            dir.clone(),
            "folder",
        ),
    ] {
        let o = m.ter_in(&uri, &args, &cwd);
        let out = stdout(&o);
        assert!(o.status.success(), "{args:?}: {out}\n{}", stderr(&o));
        assert!(
            out.contains("banner  serial_contains  graded on a board\n"),
            "{out}"
        );
        assert!(
            out.ends_with(
                "Passes on your board: `ter run --hw`. Its modes allow simulation, but it has \
                 no Wokwi circuit (diagram.json), so `ter run --sim` can't run it.\n"
            ),
            "{out}"
        );
        let mut json_args = args.clone();
        json_args.push("--json");
        let o = m.ter_in(&uri, &json_args, &cwd);
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["source"], source);
        assert_eq!(v["modes"], json!(["hardware", "simulation"]));
        assert_eq!(v["passes_in"], json!(["hardware"]));
    }
    // Not fetched: the site's file list says the same.
    let other = Machine::new();
    let (ok, v) = other.json(&site.uri(), &["ex", "show", id]);
    assert!(ok, "{v}");
    assert_eq!(v["source"], "site");
    assert_eq!(v["passes_in"], json!(["hardware"]));
    assert!(
        v["summary"].as_str().unwrap().contains("no Wokwi circuit"),
        "{v}"
    );

    // Pin checks with no circuit: no run passes it, and nothing says --sim does.
    let pins = "pins--xiao-esp32c3-nostd";
    let site = site_with_circuit(pins, Some(PIN_CHECK), &["hardware", "simulation"], false).await;
    let o = other.ter(&site.uri(), &["ex", "show", pins]);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("No ter run can pass it yet"), "{out}");
    assert!(!out.contains("Passes"), "{out}");
    assert!(!out.contains("graded only in simulation"), "{out}");
}

#[tokio::test]
async fn show_with_no_check_yaml_says_there_is_nothing_to_pass() {
    let id = "free--xiao-esp32c3-nostd";
    let site = site_with(id, None, &["hardware", "simulation"]).await;
    let m = Machine::new();
    let o = m.ter(&site.uri(), &["ex", "show", id]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stdout(&o).ends_with("No check: nothing to pass.\n"),
        "{}",
        stdout(&o)
    );
    let (ok, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert!(ok);
    assert_eq!(v["has_check"], false);
    assert_eq!(v["passes_in"], json!([]));
}

#[tokio::test]
async fn show_passes_the_sites_refusals_through() {
    let site = mock_site().await;
    let m = Machine::new();
    let o = m.ter(
        &site.uri(),
        &["ex", "show", "gpio-button--xiao-esp32c3-nostd"],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).starts_with("error[locked]"), "{}", stderr(&o));
    assert!(m.is_empty());
}

const SIM86_BOARD: &str = r#"{"parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"},
  {"id": "led1", "type": "Led"}], "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"]]}"#;

/// sim86 is listed for an exercise only when its folder has the sim86
/// board and pin map and the board shows every check.
#[tokio::test]
async fn show_lists_sim86_only_for_a_board_that_shows_every_check() {
    let id = "pins--xiao-esp32c3-nostd";
    let modes = ["hardware", "simulation"];
    let wired = [
        ("sim86/board.json", SIM86_BOARD),
        ("sim86/pins.json", r#"{"user_led": "mcu.D1"}"#),
    ];

    // Beside a Wokwi circuit: both simulators, by id and in the folder.
    let site = site_with_files(id, Some(PIN_CHECK), &modes, true, &wired).await;
    let m = Machine::new();
    let (ok, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert!(ok, "{v}");
    assert_eq!(v["source"], "site");
    assert_eq!(v["sim_venues"], json!(["wokwi", "sim86"]));
    assert_eq!(v["passes_in"], json!(["simulation"]));
    let o = m.ter(&site.uri(), &["ex", "fetch", id]);
    assert!(o.status.success(), "{}", stderr(&o));
    let dir = m.exercise_dir("unlisted", id);
    assert!(dir.join("sim86/board.json").is_file() && dir.join("sim86/pins.json").is_file());
    let o = m.ter_in("http://127.0.0.1:9", &["ex", "show"], &dir);
    let out = stdout(&o);
    assert!(o.status.success(), "{out}\n{}", stderr(&o));
    assert!(out.contains("  sims    wokwi, sim86\n"), "{out}");
    assert!(
        out.ends_with(
            "Passes only in simulation: `ter run --sim`. A run on a board can't complete it. \
             sim86 runs it too: `ter run --sim sim86`.\n"
        ),
        "{out}"
    );

    // With no Wokwi circuit, sim86 is the simulator.
    let site = site_with_files(id, Some(PIN_CHECK), &modes, false, &wired).await;
    let (_, v) = Machine::new().json(&site.uri(), &["ex", "show", id]);
    assert_eq!(v["sim_venues"], json!(["sim86"]));
    assert_eq!(v["passes_in"], json!(["simulation"]));
    assert_eq!(
        v["summary"],
        "Passes only in simulation: `ter run --sim`. A run on a board can't complete it. \
         The simulator is sim86 (`ter install sim86` sets it up)."
    );

    // The checked pin has no wire on the sim86 board: not listed, and why.
    let unwired = [
        ("sim86/board.json", SIM86_BOARD),
        ("sim86/pins.json", r#"{"user_led": "mcu.D2"}"#),
    ];
    let site = site_with_files(id, Some(PIN_CHECK), &modes, true, &unwired).await;
    let (_, v) = Machine::new().json(&site.uri(), &["ex", "show", id]);
    assert_eq!(v["sim_venues"], json!(["wokwi"]));
    let summary = v["summary"].as_str().unwrap();
    assert!(
        summary.contains("sim86 does not run it") && summary.contains("user_led is mcu.D2"),
        "{summary}"
    );
    // And with no Wokwi circuit either, no run passes it.
    let site = site_with_files(id, Some(PIN_CHECK), &modes, false, &unwired).await;
    let (_, v) = Machine::new().json(&site.uri(), &["ex", "show", id]);
    assert_eq!(v["sim_venues"], json!([]));
    assert_eq!(v["passes_in"], json!([]));

    // One of the two files alone is no sim86 board: nothing is said of it.
    let half = [("sim86/board.json", SIM86_BOARD)];
    let site = site_with_files(id, Some(PIN_CHECK), &modes, true, &half).await;
    let m = Machine::new();
    let (_, v) = m.json(&site.uri(), &["ex", "show", id]);
    assert_eq!(v["sim_venues"], json!(["wokwi"]));
    assert!(!v["summary"].as_str().unwrap().contains("sim86"), "{v}");
    let o = m.ter(&site.uri(), &["ex", "show", id]);
    assert!(!stdout(&o).contains("sim86"), "{}", stdout(&o));
}
