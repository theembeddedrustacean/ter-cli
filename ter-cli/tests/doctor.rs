//! `ter doctor` on a machine made of fake programs: what it reports, what
//! `ter install` line it gives, how it exits, and that no token shows.
//!
//! The boards on the machine running the tests are real, so nothing here
//! depends on what `usb.*` and `linux.*` find.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "device-token-that-must-not-show";
const WOKWI_TOKEN: &str = "wok_0123456789abcdefghij0123456789abcdefghij";

const RUSTUP: &str = r#"#!/bin/sh
case "$1 $2" in
  "toolchain list") echo "stable-x86_64-unknown-linux-gnu (default)" ;;
  "target list") cat "$FAKE/targets" 2>/dev/null ;;
  "component list") echo "rust-src" ;;
  *) exit 1 ;;
esac
"#;

/// A machine with a fake rustup and cargo, an empty home and no keychain.
struct Machine {
    root: tempfile::TempDir,
    site: String,
}

impl Machine {
    fn new() -> Self {
        let m = Self {
            root: tempfile::tempdir().unwrap(),
            // Nothing listens here: a test that asks the site says so.
            site: "http://127.0.0.1:9".into(),
        };
        std::fs::create_dir_all(m.bin()).unwrap();
        std::fs::create_dir_all(m.config()).unwrap();
        m.program("rustup", RUSTUP);
        m.program("cargo", "#!/bin/sh\nexit 1\n");
        m
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn program(&self, name: &str, script: &str) {
        let path = self.bin().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A tool that prints `<name> <version>`.
    fn tool(&self, name: &str, version: &str) {
        self.program(name, &format!("#!/bin/sh\necho \"{name} {version}\"\n"));
    }

    fn has_target(&self, triple: &str) {
        std::fs::write(self.root.path().join("targets"), format!("{triple}\n")).unwrap();
    }

    fn ter(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ter"));
        cmd.args(args)
            .current_dir(dir)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("FAKE", self.root.path())
            .env("HOME", self.root.path())
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .env("TER_KEYCHAIN", "off")
            .env("TER_CONFIG_DIR", self.config())
            .env("TER_TOOLS_DIR", self.root.path().join("tools"))
            .env("TER_SITE_URL", &self.site);
        for var in [
            "TER_TOKEN",
            "SIM86_API_KEY",
            "WOKWI_CLI_TOKEN",
            "TER_PORT",
            "TER_UF2_DRIVE",
            "TER_COURSES_ROOT",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn doctor(&self, dir: &Path, args: &[&str]) -> Output {
        self.ter(dir, &[&["doctor"], args].concat())
            .env("TER_TOKEN", TOKEN)
            .output()
            .unwrap()
    }
}

const C3_CONFIG: &str = "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n\n[build]\ntarget = \"riscv32imc-unknown-none-elf\"\n";

/// A fetched ESP32-C3 exercise with no check.yaml, set to run in `mode`.
fn exercise(mode: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
    std::fs::write(dir.path().join(".cargo/config.toml"), C3_CONFIG).unwrap();
    std::fs::write(
        dir.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"stable\"\ncomponents = [\"rust-src\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("ter.toml"),
        format!(
            "exercise = \"gpio-blinky--xiao-esp32c3-nostd\"\ncourse = \"esp-gpio\"\ntarget = \"xiao-esp32c3-nostd\"\nmodes = [\"hardware\", \"simulation\"]\nmode = \"{mode}\"\nfetched_sha256 = \"\"\n"
        ),
    )
    .unwrap();
    dir
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn report(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", text(&o.stdout)))
}

fn check<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no check {id}"))
}

fn assert_no_token(o: &Output) {
    for out in [&o.stdout, &o.stderr] {
        let out = text(out);
        assert!(!out.contains(TOKEN), "token printed: {out}");
        assert!(!out.contains(WOKWI_TOKEN), "Wokwi token printed: {out}");
    }
}

#[test]
fn without_an_account_the_json_report_and_the_error_both_come_out() {
    let m = Machine::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m
        .ter(elsewhere.path(), &["doctor", "--offline", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let r = report(&out);
    assert_eq!(r["ok"], false);
    assert_eq!(r["context"]["kind"], "none");
    assert_eq!(r["context"]["mode"], Value::Null);
    assert_eq!(r["install"], json!([]));
    let ids: Vec<&str> = r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "ter.version",
            "site.reach",
            "account.token",
            "account.premium",
            "rust.rustup",
            "rust.cargo",
            "rust.toolchain",
            "rust.targets",
            "tool.espflash",
            "tool.probe-rs",
            "tool.elf2uf2-rs",
            "tool.uf2deploy",
            "tool.xiao-generate",
            "tool.wokwi-cli",
            "tool.sim86",
            "wokwi.token",
            "sim86.account",
            "courses.root",
            "usb.boards",
            "usb.probes",
            "linux.serial",
            "linux.probe-usb",
            "exercise.venue",
        ]
    );
    let token = check(&r, "account.token");
    assert_eq!(token["status"], "fail");
    assert_eq!(token["group"], "account");
    assert_eq!(token["fix"], "ter login (or set TER_TOKEN)");
    assert_eq!(check(&r, "ter.version")["status"], "skip");
    assert_eq!(check(&r, "rust.rustup")["status"], "ok");
    assert_eq!(
        check(&r, "rust.toolchain")["found"],
        "stable-x86_64-unknown-linux-gnu"
    );
    assert_eq!(check(&r, "tool.espflash")["status"], "skip");
    assert_eq!(check(&r, "courses.root")["status"], "ok");
    let failed: Vec<&Value> = r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["status"] == "fail")
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "only the account can fail here: {failed:?}"
    );

    // The error envelope is on stderr, so stdout stays one document.
    let err: Value = serde_json::from_str(text(&out.stderr).trim()).unwrap();
    assert_eq!(err["error"]["code"], "not_ready");
}

#[test]
fn a_simulation_exercise_is_told_what_ter_install_will_put_right() {
    let m = Machine::new();
    let dir = exercise("simulation");
    let out = m.doctor(dir.path(), &["--offline", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let r = report(&out);
    assert_eq!(r["context"]["kind"], "exercise");
    assert_eq!(r["context"]["mode"], "simulation");
    assert_eq!(r["context"]["venue"], "wokwi");
    assert_eq!(r["install"], json!(["targets", "wokwi-cli"]));
    assert_eq!(check(&r, "account.token")["status"], "ok");
    assert_eq!(check(&r, "account.token")["found"], "from TER_TOKEN");
    let targets = check(&r, "rust.targets");
    assert_eq!(targets["status"], "fail");
    assert_eq!(
        targets["found"],
        "missing: Rust target riscv32imc-unknown-none-elf (stable)"
    );
    assert_eq!(targets["fix"], "ter install targets");
    assert_eq!(check(&r, "tool.wokwi-cli")["status"], "fail");
    assert_eq!(check(&r, "wokwi.token")["status"], "fail");
    assert_eq!(
        check(&r, "tool.espflash")["status"],
        "skip",
        "a simulation run flashes nothing"
    );
    let venue = check(&r, "exercise.venue");
    assert_eq!(venue["status"], "fail");
    assert_eq!(venue["found"], "simulation in Wokwi");
    assert_eq!(
        venue["why"],
        "not ready: rust.targets, tool.wokwi-cli, wokwi.token"
    );
    assert_no_token(&out);

    // The same, for people.
    let out = m.doctor(dir.path(), &["--offline"]);
    assert_eq!(out.status.code(), Some(1));
    let shown = text(&out.stdout);
    for line in [
        ": an exercise, run in simulation mode on wokwi\n",
        "\ntools\n",
        "  FAIL  tool.wokwi-cli      not installed\n                            fix: ter install wokwi-cli\n",
        "  -     tool.espflash       not installed\n",
        "  ok    account.token       from TER_TOKEN\n",
        "\nInstall what is missing: ter install targets wokwi-cli\n",
    ] {
        assert!(shown.contains(line), "no {line:?} in:\n{shown}");
    }
    assert!(
        text(&out.stderr).contains("error[not_ready]: 4 problems on this machine."),
        "{}",
        text(&out.stderr)
    );
    assert_no_token(&out);

    // With the target, wokwi-cli and a token there, the run is ready.
    m.has_target("riscv32imc-unknown-none-elf");
    m.tool("wokwi-cli", "0.27.1");
    let out = m
        .ter(dir.path(), &["doctor", "--offline", "--json"])
        .env("TER_TOKEN", TOKEN)
        .env("WOKWI_CLI_TOKEN", WOKWI_TOKEN)
        .output()
        .unwrap();
    let r = report(&out);
    assert_eq!(out.status.code(), Some(0), "{r:#}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["install"], json!([]));
    assert_eq!(check(&r, "wokwi.token")["found"], "from WOKWI_CLI_TOKEN");
    assert_eq!(check(&r, "exercise.venue")["status"], "ok");
    assert!(text(&out.stderr).is_empty(), "{}", text(&out.stderr));
    assert_no_token(&out);

    // A token of the wrong shape is a warning, and still exit 0.
    let out = m
        .ter(dir.path(), &["doctor", "--offline", "--json"])
        .env("TER_TOKEN", TOKEN)
        .env("WOKWI_CLI_TOKEN", "not-a-wokwi-token")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(check(&report(&out), "wokwi.token")["status"], "warn");
}

#[test]
fn tools_below_ters_floor_are_named_with_their_versions() {
    let m = Machine::new();
    m.has_target("riscv32imc-unknown-none-elf");
    m.tool("espflash", "3.3.0");
    m.tool("xiao-generate", "0.1.0");
    m.tool("probe-rs", "0.32.0");
    let dir = exercise("hardware");
    let out = m.doctor(dir.path(), &["--offline", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let r = report(&out);
    assert_eq!(r["context"]["venue"], "local");
    let espflash = check(&r, "tool.espflash");
    assert_eq!(espflash["status"], "fail");
    assert_eq!(
        espflash["found"],
        "espflash 3.3.0 is too old: ter needs 4.0.0 or newer"
    );
    assert_eq!(espflash["fix"], "ter install espflash");
    let generator = check(&r, "tool.xiao-generate");
    assert_eq!(generator["status"], "fail");
    assert_eq!(
        generator["found"],
        "xiao-generate 0.1.0 is too old: ter needs 0.1.1 or newer"
    );
    assert_eq!(check(&r, "tool.probe-rs")["status"], "ok");
    assert_eq!(check(&r, "rust.targets")["status"], "ok");
    let install: Vec<&str> = r["install"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(install, ["xiao-generate", "espflash"]);
}

#[test]
fn dev_compares_the_tools_with_a_curriculums_versions() {
    let m = Machine::new();
    m.tool("xiao-generate", "0.1.2");
    m.tool("espflash", "4.6.0");
    let curriculum = tempfile::tempdir().unwrap();
    std::fs::write(
        curriculum.path().join("toolchain.yaml"),
        "tools:\n  espflash: \"4.5\"\n  probe-rs: \"0.32\"\n  xiao-generate: \"0.1.1\"\n",
    )
    .unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let dev = curriculum.path().to_str().unwrap();
    let out = m.doctor(elsewhere.path(), &["--offline", "--json", "--dev", dev]);
    let r = report(&out);
    assert_eq!(out.status.code(), Some(0), "{r:#}");
    let generator = check(&r, "tool.xiao-generate");
    assert_eq!(generator["status"], "warn");
    assert_eq!(
        generator["why"],
        "the curriculum is checked with xiao-generate 0.1.1"
    );
    assert_eq!(check(&r, "tool.espflash")["status"], "ok");

    let out = m.doctor(elsewhere.path(), &["--offline", "--json"]);
    assert_eq!(check(&report(&out), "tool.xiao-generate")["status"], "ok");

    let out = m.doctor(
        elsewhere.path(),
        &["--offline", "--dev", elsewhere.path().to_str().unwrap()],
    );
    assert!(
        text(&out.stderr).contains("error[bad_curriculum]"),
        "{}",
        text(&out.stderr)
    );
}

async fn site(latest: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/method/ter_courses.api.ping"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"message": {
            "ok": true,
            "user": "learner@example.com",
            "server_time": "2026-11-18T12:00:00",
            "min_supported_version": "0.1.0",
            "latest_version": latest,
            "download_url": null,
            "premium": true
        }})))
        .expect(0..=1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": {"error": {
                "code": "token_invalid",
                "message": "Token revoked on the Devices page."
            }}})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn the_site_is_asked_once_and_says_who_and_which_version() {
    let server = site("99.0.0").await;
    let mut m = Machine::new();
    m.site = server.uri();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m.doctor(elsewhere.path(), &["--json"]);
    let r = report(&out);
    assert_eq!(out.status.code(), Some(0), "{r:#}");
    assert_eq!(check(&r, "site.reach")["status"], "ok");
    assert_eq!(
        check(&r, "account.token")["found"],
        "learner@example.com, from TER_TOKEN"
    );
    assert_eq!(check(&r, "account.premium")["status"], "ok");
    let version = check(&r, "ter.version");
    assert_eq!(version["status"], "warn");
    assert_eq!(version["why"], "ter 99.0.0 is available");
    assert_eq!(version["fix"], "ter self-update");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_no_token(&out);

    // The site is not asked with --offline.
    let out = m.doctor(elsewhere.path(), &["--json", "--offline"]);
    assert_eq!(check(&report(&out), "ter.version")["status"], "skip");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_the_site_refuses_is_reported_and_left_where_it_is() {
    let server = site(env!("CARGO_PKG_VERSION")).await;
    let mut m = Machine::new();
    m.site = server.uri();
    let stored = m.config().join("token");
    std::fs::write(&stored, "a-revoked-device-token").unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m
        .ter(elsewhere.path(), &["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let r = report(&out);
    let token = check(&r, "account.token");
    assert_eq!(token["status"], "fail");
    assert_eq!(token["found"], "from file, refused by the site");
    assert_eq!(token["why"], "Token revoked on the Devices page.");
    assert_eq!(token["fix"], "ter login");
    assert_eq!(check(&r, "site.reach")["status"], "ok");
    assert_eq!(
        std::fs::read_to_string(&stored).unwrap(),
        "a-revoked-device-token",
        "doctor only reports"
    );
    assert!(!text(&out.stdout).contains("a-revoked-device-token"));
    assert!(!text(&out.stderr).contains("a-revoked-device-token"));
}

#[test]
fn a_site_that_does_not_answer_is_a_reach_problem_not_an_account_one() {
    let m = Machine::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m.doctor(elsewhere.path(), &["--json"]);
    assert_eq!(out.status.code(), Some(1));
    let r = report(&out);
    let reach = check(&r, "site.reach");
    assert_eq!(reach["status"], "fail");
    assert_eq!(reach["found"], "http://127.0.0.1:9");
    assert_eq!(check(&r, "account.token")["status"], "skip");
    assert_eq!(check(&r, "ter.version")["status"], "skip");
    assert_no_token(&out);
}

#[test]
fn venues_still_answers_in_the_same_shape() {
    let m = Machine::new();
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m
        .ter(elsewhere.path(), &["venues", "--json"])
        .env("WOKWI_CLI_TOKEN", WOKWI_TOKEN)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v = report(&out);
    let keys = |v: &Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    assert_eq!(keys(&v), ["instruments", "venues"]);
    let local = &v["venues"][0];
    assert_eq!(
        keys(local),
        [
            "boards",
            "mode",
            "name",
            "port_env",
            "ready",
            "tools",
            "uf2_drives",
            "uf2_unmounted"
        ]
    );
    assert_eq!(local["name"], "local");
    assert_eq!(local["tools"], json!({"espflash": null, "probe-rs": null}));
    let wokwi = &v["venues"][1];
    assert_eq!(
        keys(wokwi),
        ["mode", "name", "provides", "ready", "token", "wokwi_cli"]
    );
    assert_eq!(wokwi["token"], true);
    assert_eq!(wokwi["wokwi_cli"], Value::Null);
    assert_eq!(wokwi["ready"], false);
    let sim86 = &v["venues"][2];
    assert_eq!(keys(sim86), ["mode", "name", "provides", "ready", "reason"]);
    assert_eq!(sim86["name"], "sim86");
    assert_eq!(sim86["mode"], "simulation");
    assert_eq!(sim86["ready"], false);
    assert_eq!(
        sim86["reason"],
        "sim86 is not installed. `ter install sim86` installs it, and needs Python 3.9 or newer on this machine; it runs on your own sim86 account."
    );
    assert_no_token(&out);
}

/// Plays the Python that holds sim86, running ter's helper: the package
/// is 0.1.4, and the account is what `$FAKE/sim86-balance` says.
const SIM86_PYTHON: &str = r#"#!/bin/sh
case "$2" in
  version) echo '{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4"}' ;;
  balance) echo asked >> "$FAKE/sim86-asked"; cat "$FAKE/sim86-balance" ;;
esac
"#;

#[test]
fn a_sim86_exercise_is_judged_for_the_package_the_key_and_the_credits() {
    let m = Machine::new();
    m.has_target("riscv32imc-unknown-none-elf");
    m.tool("espflash", "4.6.0");
    let dir = exercise("simulation");
    let folder = dir.path();
    let mut ter = std::fs::read_to_string(folder.join("ter.toml")).unwrap();
    ter.push_str("venue = \"sim86\"\n");
    std::fs::write(folder.join("ter.toml"), ter).unwrap();
    std::fs::write(
        folder.join("check.yaml"),
        "timeout_ms: 2000\nassert:\n  - id: blink\n    pin: user_led\n    toggles_per_s: { min: 1, max: 9 }\n    window_ms: [0, 1900]\n",
    )
    .unwrap();
    std::fs::create_dir_all(folder.join("sim86")).unwrap();
    std::fs::write(
        folder.join("sim86/board.json"),
        r#"{"parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"}, {"id": "led1", "type": "Led"}],
            "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"]]}"#,
    )
    .unwrap();
    std::fs::write(folder.join("sim86/pins.json"), r#"{"user_led": "mcu.D1"}"#).unwrap();

    // Not installed: `ter install sim86` puts it right.
    let out = m.doctor(folder, &["--offline", "--json"]);
    let r = report(&out);
    assert_eq!(out.status.code(), Some(1), "{r:#}");
    assert_eq!(r["context"]["venue"], "sim86");
    assert_eq!(r["install"], json!(["sim86"]));
    assert_eq!(check(&r, "tool.sim86")["status"], "fail");
    assert_eq!(check(&r, "tool.sim86")["fix"], "ter install sim86");
    assert_eq!(check(&r, "tool.wokwi-cli")["status"], "skip");

    // Installed, with a key sim86 accepts.
    let bin = m.root.path().join("tools/sim86/venv/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("python"), SIM86_PYTHON).unwrap();
    std::fs::set_permissions(bin.join("python"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let balance = |text: &str| std::fs::write(m.root.path().join("sim86-balance"), text).unwrap();
    balance(
        r#"{"helper": 1, "ok": true, "sim86": "0.1.4", "balance": {"credits": 87, "plan": "free"}}"#,
    );
    let key = "sk-sim86-never-shown";
    let doctor = |args: &[&str]| {
        m.ter(folder, &[&["doctor"], args].concat())
            .env("TER_TOKEN", TOKEN)
            .env("SIM86_API_KEY", key)
            .output()
            .unwrap()
    };
    // Offline, sim86 is not asked.
    let out = doctor(&["--offline", "--json"]);
    let r = report(&out);
    assert_eq!(out.status.code(), Some(0), "{r:#}");
    assert_eq!(check(&r, "tool.sim86")["found"], "0.1.4, Python 3.12.3");
    assert_eq!(check(&r, "sim86.account")["found"], "not asked");
    assert!(!m.root.path().join("sim86-asked").exists());
    assert_eq!(check(&r, "exercise.venue")["found"], "simulation on sim86");

    // Asked once, sim86 says the plan and the credits. (Nothing answers
    // as the site here, which is its own line of the report.)
    let out = doctor(&["--json"]);
    let r = report(&out);
    assert_eq!(
        check(&r, "sim86.account")["found"],
        "key accepted: plan free, 87 credits"
    );
    assert_eq!(check(&r, "sim86.account")["status"], "ok");
    let asked = std::fs::read_to_string(m.root.path().join("sim86-asked")).unwrap();
    assert_eq!(asked.lines().count(), 1);
    for out in [&out.stdout, &out.stderr] {
        assert!(!text(out).contains(key), "the sim86 key never shows");
    }
    let shown = text(&doctor(&[]).stdout);
    for line in [
        "  ok    tool.sim86          0.1.4, Python 3.12.3\n",
        "\nsim86\n  ok    sim86.account       key accepted: plan free, 87 credits\n",
        "simulation on sim86\n",
    ] {
        assert!(shown.contains(line), "no {line:?} in:\n{shown}");
    }

    // No key, and no credits: each stops this exercise's run.
    balance(
        r#"{"helper": 1, "ok": false, "error": {"kind": "no_key", "message": "no API key: run `sim86 login`"}}"#,
    );
    let r = report(&doctor(&["--json"]));
    let account = check(&r, "sim86.account");
    assert_eq!(
        (&account["status"], &account["found"]),
        (&json!("fail"), &json!("no key"))
    );
    assert_eq!(
        account["fix"],
        format!(
            "{} login (or set SIM86_API_KEY)",
            bin.join("sim86").display()
        )
    );
    balance(
        r#"{"helper": 1, "ok": true, "sim86": "0.1.4", "balance": {"credits": 0, "plan": "free"}}"#,
    );
    let r = report(&doctor(&["--json"]));
    assert_eq!(check(&r, "sim86.account")["status"], "fail");
    assert_eq!(check(&r, "exercise.venue")["status"], "fail");
    // sim86 not saying is a warning, and the venue is not stopped by it.
    balance(r#"{"helper": 1, "ok": false, "error": {"kind": "api", "message": "API error 500"}}"#);
    let r = report(&doctor(&["--json"]));
    assert_eq!(check(&r, "sim86.account")["status"], "warn");
    assert_eq!(check(&r, "sim86.account")["found"], "balance unknown");
}
