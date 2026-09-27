//! Against the live site. Ignored by default; run with
//! `scripts/check.sh --live`, which needs `TER_TOKEN` for a test account.

use std::process::Command;

use serde_json::Value;

fn ter_json(token: &str, args: &[&str]) -> (bool, Value) {
    let config = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(args)
        .arg("--json")
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_TOKEN", token)
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(!stdout.contains(token), "the token must never be printed");
    (o.status.success(), serde_json::from_str(&stdout).unwrap())
}

#[test]
#[ignore = "live site"]
fn live_whoami_with_bad_token_is_token_invalid() {
    for bad in [
        "not-a-real-token",
        "0000000000000000000000000000000000000000",
    ] {
        let (ok, v) = ter_json(bad, &["whoami"]);
        assert!(!ok);
        assert_eq!(v["error"]["code"], "token_invalid", "{v}");
    }
}

#[test]
#[ignore = "live site"]
fn live_whoami_with_valid_token() {
    let token = std::env::var("TER_TOKEN").expect("TER_TOKEN must be set for live tests");
    let (ok, v) = ter_json(&token, &["whoami"]);
    assert!(ok, "{v}");
    assert!(v["user"].as_str().is_some_and(|u| u.contains('@')), "{v}");
    assert_eq!(v["cli_version"], env!("CARGO_PKG_VERSION"));
    assert!(v["min_supported_version"].is_string());
    assert_eq!(v["supported"], true);
    assert!(v["premium"].is_boolean(), "{v}");
}

const SITE: &str = "https://learn.theembeddedrustacean.com";

fn pair_call(method: &str, function: &str, query: &str) -> Value {
    let o = Command::new("curl")
        .args(["-sS", "-X", method, "-H", "Content-Type: application/json"])
        .args(if method == "POST" {
            &["-d", "{}"][..]
        } else {
            &[][..]
        })
        .arg(format!(
            "{SITE}/api/method/ter_courses.api.{function}{query}"
        ))
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    v["message"].clone()
}

#[test]
#[ignore = "live site"]
fn live_pair_start_gives_a_pending_code() {
    let start = pair_call("POST", "pair_start", "");
    let code = start["code"].as_str().expect("a code");
    let (letters, digits) = code.split_once('-').expect("XXXX-0000");
    assert!(
        letters.len() == 4 && digits.len() == 4,
        "unexpected code shape {code}"
    );
    assert_eq!(start["expires_in"], 600);
    let poll = pair_call("GET", "pair_poll", &format!("?code={code}"));
    assert_eq!(poll, serde_json::json!({"status": "pending"}));
}

#[test]
#[ignore = "live site"]
fn live_used_code_is_expired() {
    // KFQY-5985 was approved and its token taken by `ter login` on
    // 2026-09-27. A second poll says `expired`, like an unknown code, so
    // polling cannot tell used codes from ones never issued. (The site
    // answered `consumed` until this was aligned with the contract.)
    let poll = pair_call("GET", "pair_poll", "?code=KFQY-5985");
    assert_eq!(poll, serde_json::json!({"status": "expired"}));
}

#[test]
#[ignore = "live site"]
fn live_unknown_code_is_expired() {
    let poll = pair_call("GET", "pair_poll", "?code=ZZZZ-0000");
    assert_eq!(poll, serde_json::json!({"status": "expired"}));
}

/// Waits out a code's ten minutes; runs only with TER_LIVE_SLOW=1.
#[test]
#[ignore = "live site"]
fn live_code_left_ten_minutes_expires() {
    if std::env::var("TER_LIVE_SLOW").as_deref() != Ok("1") {
        eprintln!("skipped: set TER_LIVE_SLOW=1 to wait out a pairing code");
        return;
    }
    let config = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["login", "--no-browser", "--json"])
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_KEYCHAIN", "off")
        .env_remove("TER_TOKEN")
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["error"]["code"], "pairing_expired", "{v}");
    assert!(!config.path().join("token").exists());
}

/// `ter --json` with a courses root of its own. `(success, answer, courses root)`.
fn ter_ex(token: &str, args: &[&str]) -> (bool, Value, tempfile::TempDir) {
    let config = tempfile::tempdir().unwrap();
    let courses = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("config.toml"),
        format!("courses_root = {:?}\n", courses.path()),
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(args)
        .arg("--json")
        .env("TER_CONFIG_DIR", config.path())
        .env("TER_TOKEN", token)
        .env("TER_KEYCHAIN", "off")
        .env_remove("TER_SITE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(!stdout.contains(token), "the token must never be printed");
    let v = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    (o.status.success(), v, courses)
}

fn live_token() -> String {
    std::env::var("TER_TOKEN").expect("TER_TOKEN must be set for live tests")
}

/// `enrollments` straight from the site, the token passed to curl in a
/// header file so it is not on the command line.
fn site_enrollments(token: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let headers = dir.path().join("headers");
    std::fs::write(&headers, format!("Authorization: Bearer {token}\n")).unwrap();
    let o = Command::new("curl")
        .args(["-sS", "-H"])
        .arg(format!("@{}", headers.display()))
        .arg(format!("{SITE}/api/method/ter_courses.api.enrollments"))
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    v["message"].clone()
}

#[test]
#[ignore = "live site"]
fn live_ex_list_matches_the_site() {
    let token = live_token();
    let (ok, listed, _) = ter_ex(&token, &["ex", "list"]);
    assert!(ok, "{listed}");
    let site = site_enrollments(&token);

    let from_site: Vec<(String, String, bool)> = site["courses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["lessons"].as_array().unwrap().iter().flat_map(move |l| {
                l["exercises"].as_array().unwrap().iter().map(move |e| {
                    (
                        c["course_id"].as_str().unwrap().to_string(),
                        e["exercise_id"].as_str().unwrap().to_string(),
                        l["locked"].as_bool().unwrap(),
                    )
                })
            })
        })
        .collect();
    let from_ter: Vec<(String, String, bool)> = listed["courses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["exercises"].as_array().unwrap().iter().map(move |e| {
                (
                    c["course_id"].as_str().unwrap().to_string(),
                    e["exercise_id"].as_str().unwrap().to_string(),
                    e["locked"].as_bool().unwrap(),
                )
            })
        })
        .collect();
    assert!(!from_site.is_empty(), "the test account has exercises");
    assert_eq!(from_ter, from_site);
}

// The test account is enrolled in ESP Bare Metal GPIO only and has not
// passed Heartbeat, so the next exercise is locked and the std course is
// not its own.
const LIVE_OPEN: &str = "gpio-blinky--xiao-esp32c3-nostd";
const LIVE_LOCKED: &str = "gpio-button-blink--xiao-esp32c3-nostd";
const LIVE_NOT_ENROLLED: &str = "std-gpio-blinky--xiao-esp32c3-std";

#[test]
#[ignore = "live site"]
fn live_fetch_refusals_come_from_the_site() {
    let token = live_token();
    for (id, code) in [
        (LIVE_LOCKED, "locked"),
        (LIVE_NOT_ENROLLED, "not_enrolled"),
        ("no-such-exercise", "not_found"),
    ] {
        let (ok, v, courses) = ter_ex(&token, &["ex", "fetch", id]);
        assert!(!ok, "{id}: {v}");
        assert_eq!(v["error"]["code"], code, "{id}: {v}");
        assert_eq!(std::fs::read_dir(courses.path()).unwrap().count(), 0);
    }
}

/// Fetches Heartbeat for the XIAO ESP32-C3 and builds it the way `ter`
/// does, into the course's shared target dir, against the lock file the
/// scaffold ships. Needs the toolchain the
/// scaffold's rust-toolchain.toml names (rustup installs it on first use).
///
/// The scaffold is the learner's starting point, with gaps (`todo!()`) the
/// lesson asks them to fill, so it is expected to fail in its own source
/// and nowhere else: every dependency must build. With `TER_CURRICULUM`
/// set to a curriculum checkout, the reference solution is laid over the
/// fetched scaffold and the whole thing must build, as the curriculum's CI
/// builds it.
#[test]
#[ignore = "live site"]
fn live_fetched_c3_scaffold_has_its_runner_and_builds() {
    let token = live_token();
    let (ok, v, courses) = ter_ex(&token, &["ex", "fetch", LIVE_OPEN]);
    assert!(ok, "{v}");
    assert!(
        v["runner"]
            .as_str()
            .unwrap()
            .starts_with("espflash flash --monitor"),
        "{v}"
    );
    let dir = std::path::PathBuf::from(v["path"].as_str().unwrap());
    let config = std::fs::read_to_string(dir.join(".cargo/config.toml")).unwrap();
    assert!(
        config.contains("runner = \"espflash flash --monitor"),
        "{config}"
    );

    assert!(
        dir.join("Cargo.lock").is_file(),
        "the scaffold ships its lock"
    );

    let layout = ter_sdk::project::Layout::new(courses.path());
    let ter = ter_sdk::project::TerToml::load(&dir).unwrap();
    // The site's own modes, not the fallback for a site that sends none.
    assert_eq!(v["modes"], serde_json::json!(ter.modes));
    assert_eq!(ter.modes, ["hardware", "simulation"]);
    assert_eq!(ter.mode, "hardware");
    let cargo_build = || {
        Command::new("cargo")
            .args(["build", "--locked", "--message-format=short"])
            .current_dir(&dir)
            .envs(layout.cargo_env(&ter, &dir))
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap()
    };

    let build = cargo_build();
    let log = String::from_utf8_lossy(&build.stderr).into_owned();
    let failed: Vec<&str> = log
        .lines()
        .filter_map(|l| l.strip_prefix("error: could not compile `"))
        .filter_map(|l| l.split('`').next())
        .collect();
    assert!(
        failed.iter().all(|c| *c == "exercise"),
        "a dependency failed: {log}"
    );
    assert!(
        log.lines()
            .filter(|l| l.contains(": error"))
            .all(|l| l.starts_with("src/")),
        "errors outside the learner's source: {log}"
    );
    assert!(
        layout
            .course_dir(&ter.course)
            .join(".target/riscv32imc-unknown-none-elf/debug")
            .is_dir()
    );
    assert!(!dir.join("target").exists(), "built into the course cache");
    let fetched = ter_sdk::project::Fetched {
        dir: dir.clone(),
        ter: ter.clone(),
    };
    assert!(!fetched.is_edited().unwrap(), "a build is not an edit");

    let Ok(curriculum) = std::env::var("TER_CURRICULUM") else {
        eprintln!("solution build skipped: set TER_CURRICULUM to a curriculum checkout");
        return;
    };
    lay_solution_over(&curriculum, &dir);
    let build = cargo_build();
    assert!(
        build.status.success(),
        "the solution does not build on the fetched scaffold: {}",
        String::from_utf8_lossy(&build.stderr)
    );
}

/// The reference solution's `src/`, emitted from a curriculum checkout,
/// over a fetched Heartbeat scaffold.
fn lay_solution_over(curriculum: &str, dir: &std::path::Path) {
    let emitted = tempfile::tempdir().unwrap();
    let emit = Command::new("python3")
        .args([
            "tools/exercise_sync.py",
            "--exercise",
            "gpio-blinky",
            "--emit",
        ])
        .arg(emitted.path())
        .arg("--solution")
        .current_dir(curriculum)
        .output()
        .unwrap();
    assert!(
        emit.status.success(),
        "{}",
        String::from_utf8_lossy(&emit.stderr)
    );
    copy_tree(
        &emitted.path().join(LIVE_OPEN).join("src"),
        &dir.join("src"),
    );
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// `--dev` from a synced checkout writes what the site serves: same course
/// folder, same source hash. Needs `TER_CURRICULUM`.
#[test]
#[ignore = "live site"]
fn live_dev_fetch_matches_the_site() {
    let Ok(curriculum) = std::env::var("TER_CURRICULUM") else {
        eprintln!("skipped: set TER_CURRICULUM to a curriculum checkout");
        return;
    };
    let token = live_token();
    let (ok, site, _a) = ter_ex(&token, &["ex", "fetch", LIVE_OPEN]);
    assert!(ok, "{site}");
    let (ok, dev, _b) = ter_ex("unused", &["ex", "fetch", LIVE_OPEN, "--dev", &curriculum]);
    assert!(ok, "{dev}");
    assert_eq!(dev["course"], site["course"]);
    assert_eq!(dev["modes"], site["modes"]);
    // The lock is left out of the source hash, so compare it byte for byte.
    let lock = |v: &Value| {
        std::fs::read(std::path::Path::new(v["path"].as_str().unwrap()).join("Cargo.lock"))
            .expect("a Cargo.lock")
    };
    assert_eq!(
        lock(&dev),
        lock(&site),
        "the checkout's lock differs from the site's"
    );
    let hash = |v: &Value| {
        ter_sdk::project::TerToml::load(std::path::Path::new(v["path"].as_str().unwrap()))
            .unwrap()
            .fetched_sha256
    };
    assert_eq!(
        hash(&dev),
        hash(&site),
        "the checkout differs from the site"
    );
}

/// The site allows one run per second per account, so tests that post runs
/// take turns and leave a second between posts.
static POSTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn take_turn() -> std::sync::MutexGuard<'static, ()> {
    let guard = POSTING.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::sleep(std::time::Duration::from_millis(1100));
    guard
}

/// One machine for several `ter` commands: a config dir and a courses root.
struct LiveMachine {
    config: tempfile::TempDir,
    courses: tempfile::TempDir,
    token: String,
}

impl LiveMachine {
    fn new() -> Self {
        let config = tempfile::tempdir().unwrap();
        let courses = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("config.toml"),
            format!("courses_root = {:?}\n", courses.path()),
        )
        .unwrap();
        Self {
            config,
            courses,
            token: live_token(),
        }
    }

    fn json(&self, args: &[&str], cwd: &std::path::Path) -> (bool, Value) {
        let o = Command::new(env!("CARGO_BIN_EXE_ter"))
            .args(args)
            .arg("--json")
            .current_dir(cwd)
            .env("TER_CONFIG_DIR", self.config.path())
            .env("TER_TOKEN", &self.token)
            .env("TER_KEYCHAIN", "off")
            .env_remove("TER_SITE_URL")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout);
        assert!(
            !stdout.contains(&self.token),
            "the token must never be printed"
        );
        let v = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
        (o.status.success(), v)
    }
}

/// The fetched Heartbeat stub does not compile until the learner fills its
/// gaps, so a build-only run of it is a failed build. It is posted with a
/// compiler tail; the next run is the next attempt and reports the hints
/// taken in between.
#[test]
#[ignore = "live site"]
fn live_failed_build_is_posted_and_attempts_count() {
    let m = LiveMachine::new();
    let (ok, v) = m.json(&["ex", "fetch", LIVE_OPEN], m.courses.path());
    assert!(ok, "{v}");
    let dir = std::path::PathBuf::from(v["path"].as_str().unwrap());

    // Other tests post runs of the same exercise, so hold the turn through
    // both runs: the attempt between them must be this test's alone.
    let turn = take_turn();
    let (ok, first) = m.json(&["run", "--no-check"], &dir);
    assert!(!ok, "the stub does not build: {first}");
    assert_eq!(first["error"]["code"], "build_failed", "{first}");
    assert_eq!(first["build_status"], "failed");
    assert_eq!(first["check_status"], "not_run");
    assert_eq!(first["mode"], "hardware");
    let tail = first["compiler_tail"].as_str().unwrap();
    assert!(tail.contains("error"), "{tail}");
    assert!(
        first["name"].as_str().is_some_and(|n| !n.is_empty()),
        "{first}"
    );
    let attempt = first["site"]["attempt"].as_u64().expect("an attempt");
    assert!(attempt >= 1);

    // The site's rung for this run, or number 0 when it has none.
    let (ok, hint) = m.json(&["hint"], &dir);
    assert!(ok, "{hint}");
    assert_eq!(hint["run"], first["name"]);
    let number = hint["number"].as_u64().unwrap();
    assert!(!hint["text"].as_str().unwrap().is_empty(), "{hint}");
    assert_eq!(hint["hints_used"], number);

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = m.json(&["run", "--no-check"], &dir).1;
    drop(turn);
    assert_eq!(second["build_status"], "failed", "{second}");
    assert_eq!(second["site"]["attempt"], attempt + 1, "{second}");
    assert_eq!(second["hints_used"], number, "{second}");
    assert_ne!(second["name"], first["name"]);

    let (ok, status) = m.json(&["status"], &dir);
    assert!(ok, "{status}");
    assert_eq!(status["last_run"]["attempt"], attempt + 1);
    assert_eq!(status["state"], "open");
}

fn live_client() -> ter_sdk::Client {
    ter_sdk::Client::new(SITE, Some(live_token()), env!("CARGO_PKG_VERSION")).unwrap()
}

/// A run that built and did not run: no evidence for mastery.
fn not_run_record(exercise: &str, mode: &str) -> ter_sdk::run::RunRecord {
    ter_sdk::run::RunRecord {
        exercise_id: exercise.into(),
        target: "xiao-esp32c3-nostd".into(),
        mode: mode.into(),
        build_status: "passed".into(),
        check_status: "not_run".into(),
        cli_version: env!("CARGO_PKG_VERSION").into(),
        ..Default::default()
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

#[test]
#[ignore = "live site"]
fn live_second_run_within_a_second_is_rate_limited() {
    let client = live_client();
    let record = not_run_record(LIVE_OPEN, "hardware");
    let _turn = take_turn();
    let (first, second) = block_on(async {
        let first = client.post_run(&record).await;
        (first, client.post_run(&record).await)
    });
    assert!(first.expect("the first run posts").attempt >= 1);
    // The site answers `rate_limited` with HTTP 429, and its host swaps the
    // body of every 429 for an HTML page, so only the code is certain.
    let err = second.unwrap_err();
    assert_eq!(err.code(), "rate_limited", "{err}");
}

/// Needs a hardware-only exercise in a course the test account is enrolled
/// in, named by `TER_LIVE_HW_ONLY`: on the live site,
/// `sandbox-hw-only--xiao-esp32c3-nostd` (Sandbox · Embed Test). Skips
/// when it is not set.
#[test]
#[ignore = "live site"]
fn live_simulation_run_of_a_hardware_only_exercise_is_refused() {
    let Ok(exercise) = std::env::var("TER_LIVE_HW_ONLY") else {
        eprintln!("skipped: set TER_LIVE_HW_ONLY to a hardware-only exercise id");
        return;
    };
    let client = live_client();
    let _turn = take_turn();
    let err = block_on(client.post_run(&not_run_record(&exercise, "simulation"))).unwrap_err();
    assert_eq!(err.code(), "mode_not_allowed", "{err}");
}

/// A good build is posted as built, not checked, with the program's hash
/// and the build time. Needs `TER_CURRICULUM` for the solution.
#[test]
#[ignore = "live site"]
fn live_good_build_is_posted_with_its_hash() {
    let Ok(curriculum) = std::env::var("TER_CURRICULUM") else {
        eprintln!("skipped: set TER_CURRICULUM to a curriculum checkout");
        return;
    };
    let m = LiveMachine::new();
    let (ok, v) = m.json(&["ex", "fetch", LIVE_OPEN], m.courses.path());
    assert!(ok, "{v}");
    let dir = std::path::PathBuf::from(v["path"].as_str().unwrap());
    lay_solution_over(&curriculum, &dir);

    let v = {
        let _turn = take_turn();
        let (ok, v) = m.json(&["run", "--no-check"], &dir);
        assert!(ok, "the solution builds and posts: {v}");
        v
    };
    assert_eq!(v["build_status"], "passed", "{v}");
    assert_eq!(v["check_status"], "not_run");
    assert!(v.get("compiler_tail").is_none(), "{v}");
    assert!(v.get("venue").is_none(), "nothing ran: {v}");
    assert!(v["duration_ms"].as_u64().is_some_and(|d| d > 0), "{v}");
    let sha = v["elf_sha256"].as_str().expect("the program's hash");
    let course = ter_sdk::project::TerToml::load(&dir).unwrap().course;
    let elf = m
        .courses
        .path()
        .join(course)
        .join(".target/riscv32imc-unknown-none-elf/debug");
    let built: Vec<_> = std::fs::read_dir(&elf)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            use sha2::Digest;
            let bytes = std::fs::read(e.path()).unwrap();
            let hex: String = sha2::Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            hex == sha
        })
        .collect();
    assert_eq!(
        built.len(),
        1,
        "the hash is of a program in {}",
        elf.display()
    );
    eprintln!(
        "posted run {} attempt {}: elf_sha256 {sha}, duration_ms {}",
        v["name"], v["site"]["attempt"], v["duration_ms"]
    );
}

/// The learner's own Wokwi token, from the environment. The Wokwi tests
/// skip without it, and without a curriculum checkout: the site does not
/// send the target's pin map yet, so they fetch with `--dev`.
fn wokwi_ready() -> Option<String> {
    let has_token = std::env::var("WOKWI_CLI_TOKEN").is_ok_and(|t| !t.is_empty());
    match (has_token, std::env::var("TER_CURRICULUM")) {
        (true, Ok(curriculum)) => Some(curriculum),
        _ => {
            eprintln!("skipped: set WOKWI_CLI_TOKEN and TER_CURRICULUM");
            None
        }
    }
}

/// Heartbeat fetched from the site into a fresh machine, with the
/// reference solution laid over it and `edit` applied to its main.rs.
fn simulated_heartbeat(
    curriculum: &str,
    edit: impl Fn(String) -> String,
) -> (LiveMachine, std::path::PathBuf) {
    let m = LiveMachine::new();
    let (ok, v) = m.json(&["ex", "fetch", LIVE_OPEN], m.courses.path());
    assert!(ok, "{v}");
    let dir = std::path::PathBuf::from(v["path"].as_str().unwrap());
    // The site sends the target's pin map; the checks' pins resolve by it.
    let ter = ter_sdk::project::TerToml::load(&dir).unwrap();
    assert_eq!(
        ter.pins.get("user_led").map(String::as_str),
        Some("GPIO3"),
        "{:?}",
        ter.pins
    );
    lay_solution_over(curriculum, &dir);
    let main = dir.join("src/bin/main.rs");
    let source = std::fs::read_to_string(&main).unwrap();
    std::fs::write(&main, edit(source)).unwrap();
    (m, dir)
}

fn telemetry_check(m: &LiveMachine, dir: &std::path::Path, run: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["telemetry", "check", run, "--check", "check.yaml"])
        .current_dir(dir)
        .env("TER_CONFIG_DIR", m.config.path())
        .env_remove("TER_TOKEN")
        .output()
        .unwrap()
}

/// The reference solution passes in Wokwi, the site answers with concept
/// deltas, and the recording re-checks to the same check.toml offline.
#[test]
#[ignore = "live site and Wokwi"]
fn live_sim_solution_passes_and_replays() {
    let Some(curriculum) = wokwi_ready() else {
        return;
    };
    let (m, dir) = simulated_heartbeat(&curriculum, |s| s);
    let v = {
        let _turn = take_turn();
        m.json(&["run", "--sim"], &dir).1
    };
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["venue"], "wokwi");
    assert_eq!(v["check_status"], "passed", "{v}");
    assert_eq!(
        (&v["checks_seen"], &v["checks_total"]),
        (&serde_json::json!(2), &serde_json::json!(2))
    );
    assert!(
        !v["site"]["concept_deltas"].as_array().unwrap().is_empty(),
        "a pass is evidence: {v}"
    );
    let run = std::path::Path::new(v["recording"].as_str().unwrap());
    let replay = telemetry_check(&m, &dir, run.to_str().unwrap());
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        replay.stdout,
        std::fs::read(run.join("check.toml")).unwrap()
    );
    eprintln!(
        "posted run {} attempt {}: {}",
        v["name"], v["site"]["attempt"], v["verdicts"]
    );
}

/// Pauses of 300 ms instead of 700: the flashes are right and the rate is
/// not, so the rate check is the one that fails.
#[test]
#[ignore = "live site and Wokwi"]
fn live_sim_wrong_solution_fails_the_right_check() {
    let Some(curriculum) = wokwi_ready() else {
        return;
    };
    let (m, dir) = simulated_heartbeat(&curriculum, |s| {
        assert!(s.contains("(Level::Low, 700)"), "the solution changed");
        s.replace("(Level::Low, 700)", "(Level::Low, 300)")
    });
    let (ok, v) = {
        let _turn = take_turn();
        m.json(&["run", "--sim"], &dir)
    };
    assert!(!ok);
    assert_eq!(v["error"]["code"], "check_failed", "{v}");
    assert_eq!(v["check_status"], "failed");
    let ff: Value = serde_json::from_str(v["first_failure"].as_str().unwrap()).unwrap();
    assert_eq!(ff["id"], "heartbeat-rate", "{ff}");
    assert_eq!(
        v["verdicts"][1]["status"], "pass",
        "the flashes are right: {v}"
    );
}

/// With no check.yaml, `--sim` builds only and posts `not_run` with no
/// checks, and Wokwi is never called.
#[test]
#[ignore = "live site"]
fn live_sim_without_a_check_builds_only() {
    let Ok(curriculum) = std::env::var("TER_CURRICULUM") else {
        eprintln!("skipped: set TER_CURRICULUM to a curriculum checkout");
        return;
    };
    let (m, dir) = simulated_heartbeat(&curriculum, |s| s);
    std::fs::remove_file(dir.join("check.yaml")).unwrap();
    let (ok, v) = {
        let _turn = take_turn();
        m.json(&["run", "--sim"], &dir)
    };
    assert!(ok, "{v}");
    assert_eq!(v["check_status"], "not_run");
    assert_eq!(
        (&v["checks_seen"], &v["checks_total"]),
        (&serde_json::json!(0), &serde_json::json!(0))
    );
    assert!(v.get("venue").is_none(), "{v}");
}

/// A token Wokwi refuses is the venue's failure, not the learner's: posted
/// as `not_run` with `venue_unavailable` first in the transcript.
#[test]
#[ignore = "live site and Wokwi"]
fn live_sim_refused_wokwi_token_is_not_run() {
    let Some(curriculum) = wokwi_ready() else {
        return;
    };
    let (m, dir) = simulated_heartbeat(&curriculum, |s| s);
    let o = {
        let _turn = take_turn();
        Command::new(env!("CARGO_BIN_EXE_ter"))
            .args(["run", "--sim", "--json"])
            .current_dir(&dir)
            .env("TER_CONFIG_DIR", m.config.path())
            .env("TER_TOKEN", &m.token)
            .env("TER_KEYCHAIN", "off")
            .env("WOKWI_CLI_TOKEN", "not-a-wokwi-token")
            .output()
            .unwrap()
    };
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(!o.status.success());
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert_eq!(v["check_status"], "not_run", "never failed: {v}");
    let tail = v["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"), "{tail}");
}

/// The bench tests need a bare XIAO ESP32-C3 on this machine's USB (and the
/// user in `dialout`); they skip unless `TER_BENCH=1`. Every run posts to
/// the test account.
fn bench_ready() -> bool {
    let ready = std::env::var("TER_BENCH").is_ok_and(|v| v == "1");
    if !ready {
        eprintln!("skipped: plug in a XIAO ESP32-C3 and set TER_BENCH=1");
    }
    ready
}

/// env-first-build prints its banner and blinks: it ships complete, so the
/// fetched scaffold is the reference program.
const LIVE_BANNER: &str = "env-first-build--xiao-esp32c3-nostd";
const LIVE_HW_ONLY_PINS: &str = "sandbox-hw-only--xiao-esp32c3-nostd";
/// One banner check, hardware and simulation.
const LIVE_SERIAL_ONLY: &str = "sandbox-serial-only--xiao-esp32c3-nostd";

fn fetched(m: &LiveMachine, id: &str) -> std::path::PathBuf {
    let (ok, v) = m.json(&["ex", "fetch", id], m.courses.path());
    assert!(ok, "{v}");
    std::path::PathBuf::from(v["path"].as_str().unwrap())
}

fn events(v: &Value) -> Vec<Value> {
    let run = std::path::Path::new(v["recording"].as_str().unwrap());
    std::fs::read_to_string(run.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A serial check passes on the board and in Wokwi with the same
/// check.yaml: sandbox-serial-only's banner, which a bare board sees in
/// full.
#[test]
#[ignore = "live site and board"]
fn bench_a_serial_check_passes_on_the_board_and_in_wokwi() {
    if !bench_ready() {
        return;
    }
    let m = LiveMachine::new();
    let dir = fetched(&m, LIVE_SERIAL_ONLY);
    let check = std::fs::read_to_string(dir.join("check.yaml")).unwrap();
    assert!(!check.contains("pin:"), "serial checks only: {check}");

    let v = {
        let _turn = take_turn();
        m.json(&["run", "--hw"], &dir).1
    };
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        (&v["mode"], &v["venue"]),
        (&"hardware".into(), &"local".into())
    );
    assert_eq!(v["check_status"], "passed", "{v}");
    assert_eq!(
        (&v["checks_seen"], &v["checks_total"]),
        (&serde_json::json!(1), &serde_json::json!(1)),
        "a full pass on a bare board"
    );
    let ev = events(&v);
    assert!(
        ev[0]["capture"]["provides"]
            .as_array()
            .unwrap()
            .contains(&"reset".into()),
        "ter resets a USB-Serial-JTAG board itself: {}",
        ev[0]
    );
    assert_eq!(ev[1]["kind"], "reset");
    let run = std::path::Path::new(v["recording"].as_str().unwrap());
    let replay = telemetry_check(&m, &dir, run.to_str().unwrap());
    assert_eq!(
        replay.stdout,
        std::fs::read(run.join("check.toml")).unwrap()
    );
    eprintln!(
        "board: run {} attempt {}: {}\n{}",
        v["name"], v["site"]["attempt"], v["verdicts"], v["transcript_tail"]
    );

    if std::env::var("WOKWI_CLI_TOKEN").is_ok_and(|t| !t.is_empty()) {
        let v = {
            let _turn = take_turn();
            m.json(&["run", "--sim"], &dir).1
        };
        assert!(v.get("error").is_none(), "{v}");
        assert_eq!(v["venue"], "wokwi");
        assert_eq!(v["check_status"], "passed", "{v}");
        eprintln!("wokwi: run {} {}", v["name"], v["verdicts"]);
    } else {
        eprintln!("Wokwi half skipped: set WOKWI_CLI_TOKEN");
    }
}

/// On a bare board a pin check is named as unseen: env-first-build's blink
/// makes its run a partial pass, which the site does not count, and
/// sandbox-hw-only (pin checks only) is posted as not run.
#[test]
#[ignore = "live site and board"]
fn bench_pin_checks_are_named_unseen_and_not_counted() {
    if !bench_ready() {
        return;
    }
    let m = LiveMachine::new();
    let dir = fetched(&m, LIVE_BANNER);
    let v = {
        let _turn = take_turn();
        m.json(&["run", "--hw"], &dir).1
    };
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["check_status"], "passed", "{v}");
    assert_eq!(
        (&v["checks_seen"], &v["checks_total"]),
        (&serde_json::json!(1), &serde_json::json!(2))
    );
    assert_eq!(v["verdicts"][1]["id"], "blink-rate");
    assert_eq!(v["verdicts"][1]["status"], "not_observable");
    let deltas = v["site"]["concept_deltas"].as_array().unwrap();
    assert!(
        deltas.iter().all(|d| d["before"] == d["after"]),
        "a partial pass is not counted: {v}"
    );
    eprintln!(
        "partial pass: run {} attempt {}",
        v["name"], v["site"]["attempt"]
    );

    let dir = fetched(&m, LIVE_HW_ONLY_PINS);
    let v = {
        let _turn = take_turn();
        m.json(&["run", "--hw"], &dir).1
    };
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(v["check_status"], "not_run", "{v}");
    assert_eq!(
        (&v["checks_seen"], &v["checks_total"]),
        (&serde_json::json!(0), &serde_json::json!(2))
    );
    eprintln!(
        "nothing seen: run {} attempt {}",
        v["name"], v["site"]["attempt"]
    );
}

/// The board's USB device, as sysfs knows it: the folder with `authorized`.
fn usb_device_of(port: &str) -> Option<std::path::PathBuf> {
    let tty = std::path::Path::new(port)
        .file_name()?
        .to_str()?
        .to_string();
    let mut dir = std::fs::canonicalize(format!("/sys/class/tty/{tty}/device")).ok()?;
    while !dir.join("authorized").is_file() || !dir.join("idVendor").is_file() {
        dir = dir.parent()?.to_path_buf();
    }
    Some(dir)
}

fn set_authorized(device: &std::path::Path, on: bool) {
    let ok = Command::new("sudo")
        .args(["-n", "tee"])
        .arg(device.join("authorized"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(if on { b"1" } else { b"0" })?;
            c.wait()
        })
        .is_ok_and(|s| s.success());
    assert!(ok, "could not switch {} (needs sudo -n)", device.display());
}

/// The board is unplugged (its USB device de-authorised, which the kernel
/// treats as a disconnect) a second into the capture: the run is posted
/// as not run with `venue_unavailable`, never as failed.
#[test]
#[ignore = "live site and board"]
fn bench_unplugging_the_board_mid_capture_is_not_run() {
    if !bench_ready() {
        return;
    }
    let port = std::env::var("TER_PORT").unwrap_or_else(|_| "/dev/ttyACM0".into());
    let Some(device) = usb_device_of(&port) else {
        panic!("no USB device behind {port}; set TER_PORT");
    };
    let m = LiveMachine::new();
    let dir = fetched(&m, LIVE_BANNER);
    let flash_log = dir.join(".runs/1/flash.log");
    let unplug = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        while !flash_log.exists() {
            assert!(started.elapsed() < std::time::Duration::from_secs(600));
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
        set_authorized(&device, false);
        device
    });
    let (ok, v) = {
        let _turn = take_turn();
        m.json(&["run", "--hw"], &dir)
    };
    let device = unplug.join().unwrap();
    set_authorized(&device, true);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert_eq!(v["check_status"], "not_run", "never failed: {v}");
    assert!(v["first_failure"].is_null() || v.get("first_failure").is_none());
    let tail = v["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"), "{tail}");
    eprintln!(
        "unplugged: run {} attempt {}\n{tail}",
        v["name"], v["site"]["attempt"]
    );
}

/// `ter serve` on a machine that has never fetched the exercise: the page
/// posts sandbox-serial-only's files (from the fork's dev server origin),
/// ter fetches the scaffold, runs it in Wokwi and answers with the run it
/// posted.
#[test]
#[ignore = "live site and Wokwi"]
fn live_serve_fetches_runs_and_posts_for_the_page() {
    if !std::env::var("WOKWI_CLI_TOKEN").is_ok_and(|t| !t.is_empty()) {
        eprintln!("skipped: set WOKWI_CLI_TOKEN");
        return;
    }
    use std::io::BufRead;
    let m = LiveMachine::new();
    // The page's files: the scaffold as the site serves it.
    let page = tempfile::tempdir().unwrap();
    let (ok, v) = m.json(
        &[
            "ex",
            "fetch",
            LIVE_SERIAL_ONLY,
            page.path().join("x").to_str().unwrap(),
        ],
        page.path(),
    );
    assert!(ok, "{v}");
    let scaffold = page.path().join("x");
    let files: Vec<Value> = ["Cargo.toml", "src/bin/main.rs", "src/lib.rs", "check.yaml"]
        .iter()
        .map(|p| {
            serde_json::json!({"path": p, "content": std::fs::read_to_string(scaffold.join(p)).unwrap()})
        })
        .collect();
    std::fs::remove_dir_all(m.courses.path()).ok();
    std::fs::create_dir_all(m.courses.path()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ter"))
        .args(["serve", "--port", "0"])
        .env("TER_CONFIG_DIR", m.config.path())
        .env("TER_TOKEN", &m.token)
        .env("TER_KEYCHAIN", "off")
        .env_remove("TER_SITE_URL")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut line).unwrap();
    assert!(!line.contains(&m.token));
    let url = line
        .strip_prefix("ter serve on ")
        .and_then(|l| l.split(',').next())
        .unwrap()
        .to_string();

    let v: Value = {
        let _turn = take_turn();
        block_on(async {
            let r = reqwest::Client::new()
                .post(format!("{url}/run"))
                .header("origin", "http://localhost:8080")
                .json(&serde_json::json!({"exercise_id": LIVE_SERIAL_ONLY, "files": files}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200);
            assert_eq!(
                r.headers()["access-control-allow-origin"],
                "http://localhost:8080"
            );
            r.json().await.unwrap()
        })
    };
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(v["venue"], "wokwi", "{v}");
    assert_eq!(v["check_status"], "passed", "{v}");
    assert!(v["name"].as_str().is_some_and(|n| !n.is_empty()), "{v}");
    assert!(v["site"]["attempt"].as_u64().is_some(), "{v}");
    assert!(
        v.get("not_written").is_none(),
        "the page sent the scaffold as is: {v}"
    );
    let fetched = std::path::Path::new(v["recording"].as_str().unwrap());
    assert!(
        fetched.starts_with(m.courses.path()),
        "serve fetched it into the courses root: {}",
        fetched.display()
    );
    eprintln!(
        "serve posted run {} attempt {}",
        v["name"], v["site"]["attempt"]
    );
}

/// `ter serve --share` as TER Smoke Test, with a bench of its own that is
/// removed from the site when this is dropped.
struct SharedBench {
    m: LiveMachine,
    child: Option<std::process::Child>,
    /// What `ter serve` has printed to stderr so far.
    said: std::sync::Arc<std::sync::Mutex<String>>,
    name: String,
    code: String,
}

impl SharedBench {
    fn start(label: &str) -> Self {
        use std::io::BufRead;
        let m = LiveMachine::new();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ter"))
            .args(["serve", "--port", "0", "--json", "--share"])
            .args([
                "--board",
                "xiao-esp32c3",
                "--label",
                label,
                "--bench-port",
                "0",
            ])
            .current_dir(m.courses.path())
            .env("TER_CONFIG_DIR", m.config.path())
            .env("TER_TOKEN", &m.token)
            .env("TER_KEYCHAIN", "off")
            .env_remove("TER_SITE_URL")
            .env_remove("TER_PORT")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn({
            let said = said.clone();
            move || {
                for line in std::io::BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                {
                    eprintln!("serve: {line}");
                    let mut s = said.lock().unwrap();
                    s.push_str(&line);
                    s.push('\n');
                }
            }
        });
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(!line.contains(&m.token), "the token must never be printed");
        let banner: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        let bench = &banner["bench"];
        eprintln!("shared: {bench}");
        Self {
            name: bench["bench"].as_str().expect("a bench name").to_string(),
            code: bench["share_code"]
                .as_str()
                .expect("a share code")
                .to_string(),
            m,
            child: Some(child),
            said,
        }
    }

    /// Wait until `ter serve` has printed `text`.
    fn wait_said(&self, text: &str, limit: std::time::Duration) -> std::time::Duration {
        let took = wait_for(&format!("serve saying {text:?}"), limit, || {
            self.said.lock().unwrap().contains(text)
        });
        assert!(
            !self.said.lock().unwrap().contains(&self.m.token),
            "the token must never be printed"
        );
        took
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// This bench as `ter bench list` shows it (the Devices page's view).
    fn listed(&self) -> Value {
        let (ok, v) = self.m.json(&["bench", "list"], self.m.courses.path());
        assert!(ok, "{v}");
        v["mine"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["bench"] == self.name.as_str())
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl Drop for SharedBench {
    fn drop(&mut self) {
        self.stop();
        let (ok, v) = self
            .m
            .json(&["bench", "remove", &self.name], self.m.courses.path());
        if !ok {
            eprintln!("could not remove live test bench {}: {v}", self.name);
        }
    }
}

fn wait_for(
    what: &str,
    limit: std::time::Duration,
    mut done: impl FnMut() -> bool,
) -> std::time::Duration {
    let t = std::time::Instant::now();
    while !done() {
        assert!(
            t.elapsed() < limit,
            "{what}: not within {} s",
            limit.as_secs()
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    t.elapsed()
}

fn live_label(what: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("ter-cli live {what} {t}")
}

/// The phase 8 flow against the site: the bench shows under Mine, stays up
/// on heartbeats, is shared, a connection shows as Connected, unsharing
/// drops it, and a stopped `ter serve` shows offline a minute later.
/// About two and a half minutes.
#[test]
#[ignore = "live site"]
fn live_serve_share_registers_heartbeats_shares_and_goes_offline() {
    let mut bench = SharedBench::start(&live_label("share"));
    let took = wait_for(
        "bench under Mine",
        std::time::Duration::from_secs(30),
        || bench.listed()["status"] == "available",
    );
    let listed = bench.listed();
    eprintln!("under Mine after {} s: {listed}", took.as_secs());
    assert_eq!(listed["board"], "xiao-esp32c3");
    assert_eq!(listed["sharing"], true);
    assert_eq!(listed["share_code"], bench.code.as_str());
    assert!(
        listed["url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:"),
        "{listed}"
    );

    let (ok, v) = bench
        .m
        .json(&["bench", "share", &bench.name], bench.m.courses.path());
    assert!(ok, "{v}");
    assert_eq!(
        v["share_code"],
        bench.code.as_str(),
        "sharing again keeps the code"
    );

    // Connect with the code from another machine: the test account itself,
    // and a second account when TER_TOKEN_2 names one.
    let driver = LiveMachine::new();
    let (ok, v) = driver.json(
        &["connect", &bench.code.to_lowercase()],
        driver.courses.path(),
    );
    assert!(ok, "{v}");
    assert_eq!(v["bench"], bench.name.as_str());
    assert_eq!(v["status"], "available");
    let mut expected = 1;
    let second = std::env::var("TER_TOKEN_2")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|token| {
            let mut other = LiveMachine::new();
            other.token = token;
            let (ok, v) = other.json(&["connect", &bench.code], other.courses.path());
            assert!(ok, "{v}");
            other
        });
    if second.is_some() {
        expected = 2;
    } else {
        eprintln!("second account skipped: set TER_TOKEN_2 to a second test account's token");
    }
    let listed = bench.listed();
    assert_eq!(
        listed["connected"].as_array().unwrap().len(),
        expected,
        "Connected on the owner's page: {listed}"
    );
    let (ok, v) = driver.json(&["bench", "list"], driver.courses.path());
    assert!(ok, "{v}");
    assert_eq!(v["connected"][0]["bench"], bench.name.as_str());
    // ter serve hears who is connected from its heartbeat.
    let took = bench.wait_said(
        "Connected to the bench now: ",
        std::time::Duration::from_secs(40),
    );
    eprintln!("serve saw the connection {} s after it", took.as_secs());

    // Heartbeats keep it up past the site's 60 s.
    std::thread::sleep(std::time::Duration::from_secs(70));
    assert_eq!(
        bench.listed()["status"],
        "available",
        "heartbeats keep it up"
    );

    let (ok, v) = bench
        .m
        .json(&["bench", "unshare", &bench.name], bench.m.courses.path());
    assert!(ok, "{v}");
    assert_eq!(v["dropped"], expected, "{v}");
    let listed = bench.listed();
    assert_eq!(listed["sharing"], false);
    assert_eq!(
        listed["connected"],
        serde_json::json!([]),
        "unshare drops them: {listed}"
    );
    // ter serve hears from its heartbeat that sharing is off, and closes
    // the bench.
    let took = bench.wait_said(
        "Sharing was turned off on the site.",
        std::time::Duration::from_secs(40),
    );
    eprintln!(
        "serve closed the bench {} s after the unshare",
        took.as_secs()
    );
    bench.wait_said(
        "Nobody is connected to the bench now.",
        std::time::Duration::from_secs(5),
    );
    let (ok, v) = driver.json(&["connect", &bench.code], driver.courses.path());
    assert!(!ok, "the old code no longer connects: {v}");
    assert_eq!(v["error"]["code"], "not_found", "{v}");

    bench.stop();
    assert_eq!(
        bench.listed()["status"],
        "available",
        "not offline the moment it stops"
    );
    let took = wait_for("offline", std::time::Duration::from_secs(75), || {
        bench.listed()["status"] == "offline"
    });
    eprintln!("offline {} s after ter serve stopped", took.as_secs());
    drop(second);
}

/// A connected bench whose owner's `ter serve` has no board refuses the
/// run before the build, over the real registry and the real share code.
#[test]
#[ignore = "live site"]
fn live_a_bench_without_a_board_refuses_before_the_build() {
    if std::env::var("TER_BENCH").is_ok_and(|v| v == "1") {
        eprintln!("skipped: a board is plugged in (TER_BENCH=1)");
        return;
    }
    let bench = SharedBench::start(&live_label("no board"));
    let driver = LiveMachine::new();
    let (ok, v) = driver.json(&["connect", &bench.code], driver.courses.path());
    assert!(ok, "{v}");
    let dir = fetched(&driver, LIVE_SERIAL_ONLY);
    let (ok, v) = driver.json(&["run", "--hw", "--venue", "bench"], &dir);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no board plugged in"),
        "the bench answered with the share code: {v}"
    );
    assert!(!dir.join(".runs").exists(), "nothing built, nothing posted");
}

/// The seeded bench (owner: the test account, code GZTS-2520) has never
/// been served, so it connects but cannot take a run.
#[test]
#[ignore = "live site"]
fn live_seeded_bench_connects_and_forgets() {
    let m = LiveMachine::new();
    let (ok, v) = m.json(&["connect", "GZTS-2520"], m.courses.path());
    assert!(ok, "{v}");
    assert_eq!(v["board"], "xiao-esp32c3");
    assert_eq!(v["status"], "offline");
    let dir = fetched(&m, LIVE_SERIAL_ONLY);
    let (ok, v) = m.json(&["run", "--hw", "--venue", "bench"], &dir);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no address"),
        "{v}"
    );
    assert!(!dir.join(".runs").exists());
    let (ok, v) = m.json(&["bench", "disconnect"], m.courses.path());
    assert!(ok, "{v}");
    let (ok, v) = m.json(&["bench", "forget"], m.courses.path());
    assert!(ok, "{v}");
    let (ok, v) = m.json(&["bench", "forget"], m.courses.path());
    assert!(!ok, "gone from the list: {v}");
}

#[test]
#[ignore = "live site"]
fn live_bench_calls_with_a_bad_token_are_token_invalid() {
    for args in [&["connect", "GZTS-2520"][..], &["bench", "share"][..]] {
        let (ok, v) = ter_json("not-a-real-token", args);
        assert!(!ok);
        assert_eq!(v["error"]["code"], "token_invalid", "{args:?}: {v}");
    }
}

/// With the XIAO on this machine: shared, connected to, and run on as a
/// bench, the serial check passes as it does on the local board.
#[test]
#[ignore = "live site"]
fn bench_a_run_on_a_shared_bench_passes_its_serial_check() {
    if !bench_ready() {
        return;
    }
    let bench = SharedBench::start(&live_label("board"));
    let driver = LiveMachine::new();
    let (ok, v) = driver.json(&["connect", &bench.code], driver.courses.path());
    assert!(ok, "{v}");
    let dir = fetched(&driver, LIVE_SERIAL_ONLY);
    let v = {
        let _turn = take_turn();
        driver.json(&["run", "--hw", "--venue", "bench"], &dir).1
    };
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(
        (&v["mode"], &v["venue"]),
        (&"hardware".into(), &"bench".into())
    );
    assert_eq!(v["check_status"], "passed", "{v}");
    assert_eq!(events(&v)[0]["capture"]["venue"], "bench");
    eprintln!(
        "bench: run {} attempt {}: {}",
        v["name"], v["site"]["attempt"], v["verdicts"]
    );
}
