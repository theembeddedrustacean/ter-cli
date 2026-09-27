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
        .current_dir(&curriculum)
        .output()
        .unwrap();
    assert!(
        emit.status.success(),
        "{}",
        String::from_utf8_lossy(&emit.stderr)
    );
    let solution = emitted.path().join(LIVE_OPEN).join("src");
    copy_tree(&solution, &dir.join("src"));
    let build = cargo_build();
    assert!(
        build.status.success(),
        "the solution does not build on the fetched scaffold: {}",
        String::from_utf8_lossy(&build.stderr)
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
