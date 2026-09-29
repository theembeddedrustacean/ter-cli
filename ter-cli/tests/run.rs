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

/// Plays `espflash`: keeps its arguments, fails with the exit code in
/// `exit-code`, and otherwise says it flashed and leaves `flashed` for the
/// test's board to start printing.
const FAKE_ESPFLASH: &str = r#"#!/bin/sh
here="$(dirname "$0")"
echo "$@" > "$here/got-args"
code="$(cat "$here/exit-code")"
if [ "$code" != 0 ]; then
  echo "Error: espflash::connection_failed" >&2
  echo "  x Error while connecting to device" >&2
  exit "$code"
fi
echo "Flashing has completed!"
touch "$here/flashed"
"#;

/// Plays the linker: writes the Arm program next to it where rustc asked
/// for the executable.
const FAKE_LINKER: &str = r#"#!/bin/sh
here="$(dirname "$0")"
for a in "$@"; do
  case "$a" in @*) set -- "$@" $(cat "${a#@}") ;; esac
done
while [ $# -gt 0 ]; do
  [ "$1" = "-o" ] && out="$2"
  shift
done
cp "$here/program.elf" "$out"
"#;

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_string()
}

/// A serial check a bare board can see and a pin check it cannot.
const BANNER_AND_BLINK: &str = "\
timeout_ms: 1500
assert:
  - id: banner
    serial_contains: \"Hello world!\"
    within_ms: 1000
  - id: blink-rate
    pin: user_led
    toggles_per_s: { min: 1.8, max: 2.2 }
    window_ms: [200, 1200]
";

const GOOD: &str = "fn main() {\n    println!(\"blink\");\n}\n";
const BROKEN: &str = "fn main() {\n    let _ = led;\n}\n";

/// Held to spawn a process, and exclusively while a test's board opens
/// its pty: `TTYPort::pair` opens it without close-on-exec, and a `ter`
/// spawned in that moment would inherit it and keep the board plugged in.
static SPAWN: std::sync::RwLock<()> = std::sync::RwLock::new(());

struct Machine {
    config: TempDir,
    courses: TempDir,
    /// A stand-in for `wokwi-cli`, first on the PATH, when set.
    fake_wokwi: Option<TempDir>,
    /// A stand-in for `espflash`, first on the PATH, when set.
    fake_espflash: Option<TempDir>,
    /// `TER_PORT`, when set.
    port: Option<String>,
    /// A UF2 bootloader's drive (`TER_UF2_DRIVE`) and the linker that
    /// makes the build an Arm program for it, when set.
    uf2: Option<TempDir>,
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
            fake_espflash: None,
            port: None,
            uf2: None,
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
        let mut cmd = self.command(site, args, cwd);
        let child = {
            let _spawning = SPAWN.read().unwrap();
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        child.wait_with_output().unwrap()
    }

    /// `ter serve` on a free port, until the returned value is dropped.
    fn serve(&self, site: &str) -> Served {
        self.serve_with(site, &[])
    }

    /// `ter serve --port 0 <extra>`, with its standard input kept open for
    /// the kill switch and its banner read.
    fn serve_with(&self, site: &str, extra: &[&str]) -> Served {
        use std::io::BufRead;
        let mut args = vec!["serve", "--port", "0"];
        args.extend_from_slice(extra);
        let mut cmd = self.command(site, &args, self.courses.path());
        let mut child = {
            let _spawning = SPAWN.read().unwrap();
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap()
        };
        let mut line = String::new();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        stdout.read_line(&mut line).unwrap();
        let url = line
            .strip_prefix("ter serve on ")
            .and_then(|l| l.split(',').next())
            .unwrap_or_else(|| panic!("{line}"))
            .to_string();
        let mut banner = line.clone();
        let lines = if extra.contains(&"--share") { 3 } else { 1 };
        for _ in 0..lines {
            let mut more = String::new();
            stdout.read_line(&mut more).unwrap();
            banner.push_str(&more);
        }
        Served {
            stdin: child.stdin.take(),
            child,
            url,
            banner,
            _stdout: stdout,
        }
    }

    fn command(&self, site: &str, args: &[&str], cwd: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ter"));
        cmd.args(args)
            .current_dir(cwd)
            .env("TER_KEYCHAIN", "off")
            .env("TER_SITE_URL", site)
            .env("TER_CONFIG_DIR", self.config.path())
            .env("TER_TOKEN", "good-token")
            .env_remove("WOKWI_CLI_TOKEN")
            .env_remove("TER_PORT")
            .env_remove("TER_UF2_DRIVE");
        if let Some(port) = &self.port {
            cmd.env("TER_PORT", port);
        }
        if let Some(uf2) = &self.uf2 {
            cmd.env("TER_UF2_DRIVE", uf2.path().join("RPI-RP2"));
        }
        if let Some(fake) = &self.fake_espflash {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs = vec![fake.path().to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(dirs).unwrap());
        }
        if let Some(fake) = &self.fake_wokwi {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs = vec![fake.path().to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(dirs).unwrap())
                .env("WOKWI_CLI_TOKEN", "fake-wokwi");
        }
        cmd
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

    /// Make the exercise one for the board: `check` as its check.yaml, an
    /// espflash runner, a fake `espflash` that exits with `exit_code`, and
    /// `port` as TER_PORT.
    #[cfg(unix)]
    fn on_board(mut self, check: &str, exit_code: i32, port: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.exercise();
        std::fs::write(dir.join("check.yaml"), check).unwrap();
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n",
        )
        .unwrap();
        let fake = tempfile::tempdir().unwrap();
        let script = fake.path().join("espflash");
        std::fs::write(&script, FAKE_ESPFLASH).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(fake.path().join("exit-code"), exit_code.to_string()).unwrap();
        self.fake_espflash = Some(fake);
        self.port = Some(port.to_string());
        self
    }

    /// Make the exercise one for an XIAO RP2040: `check` as its check.yaml,
    /// the `elf2uf2-rs -d` runner, and a build that links an RP2040
    /// program (a stand-in linker writes it). With `drive`, the board is in
    /// its bootloader: TER_UF2_DRIVE names its drive, and once ter copies
    /// the program there the drive goes away as the board restarts.
    #[cfg(unix)]
    fn on_uf2_board(mut self, check: &str, drive: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.exercise();
        std::fs::write(dir.join("check.yaml"), check).unwrap();
        let uf2 = tempfile::tempdir().unwrap();
        let linker = uf2.path().join("ld");
        std::fs::write(&linker, FAKE_LINKER).unwrap();
        std::fs::set_permissions(&linker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = ter_flash::uf2::elf32(&[(0x1000_0000, &[0x5a; 600])]);
        std::fs::write(uf2.path().join("program.elf"), program).unwrap();
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(
            dir.join(".cargo/config.toml"),
            format!(
                "[target.thumbv6m-none-eabi]\nrunner = \"elf2uf2-rs -d\"\n\n[target.{}]\nlinker = {:?}\n",
                host_triple(),
                linker
            ),
        )
        .unwrap();
        if drive {
            let drive = uf2.path().join("RPI-RP2");
            std::fs::create_dir(&drive).unwrap();
            let info = drive.join("INFO_UF2.TXT");
            std::fs::write(
                &info,
                "UF2 Bootloader v3.0\nModel: Raspberry Pi RP2\nBoard-ID: RPI-RP2\n",
            )
            .unwrap();
            let copied = drive.join("ter.uf2");
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                while std::fs::metadata(&copied).map_or(true, |m| m.len() == 0) {
                    if started.elapsed() > std::time::Duration::from_secs(300) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                let _ = std::fs::remove_file(info);
            });
        }
        self.uf2 = Some(uf2);
        self
    }

    /// The file ter copies to the UF2 drive.
    fn uf2_copied(&self) -> PathBuf {
        self.uf2.as_ref().unwrap().path().join("RPI-RP2/ter.uf2")
    }

    fn espflash_saw(&self) -> String {
        let fake = self.fake_espflash.as_ref().unwrap();
        std::fs::read_to_string(fake.path().join("got-args")).unwrap()
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

/// A running `ter serve`, stopped when dropped.
struct Served {
    child: std::process::Child,
    url: String,
    /// What it printed on start.
    banner: String,
    stdin: Option<std::process::ChildStdin>,
    /// Kept open for as long as the service runs.
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Served {
    /// POST `body` to /run as the lesson page on the site would.
    async fn run(&self, body: Value) -> (u16, reqwest::header::HeaderMap, Value) {
        let r = reqwest::Client::new()
            .post(format!("{}/run", self.url))
            .header("origin", PAGE)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        (status, headers, r.json().await.unwrap())
    }
}

const PAGE: &str = "https://learn.theembeddedrustacean.com";

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

/// A board on a pseudo-terminal: once the fake espflash has flashed, it
/// prints `lines` 100 ms apart, then, with `unplug`, goes away.
#[cfg(unix)]
struct Board {
    port: String,
    thread: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl Board {
    fn start(flashed: PathBuf, lines: &'static [&'static str], unplug: bool) -> Self {
        use serialport::SerialPort;
        use std::io::Write;
        // pair() leaves both ends open across exec, so ter would hold the
        // pty open and never see the unplug; keep close-on-exec copies.
        let (mut master, slave, port) = {
            let _alone = SPAWN.write().unwrap();
            let (master, slave) = serialport::TTYPort::pair().unwrap();
            let port = slave.name().unwrap();
            (
                master.try_clone_native().unwrap(),
                slave.try_clone_native().unwrap(),
                port,
            )
        };
        let thread = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while !flashed.exists() {
                if started.elapsed() > std::time::Duration::from_secs(300) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            for line in lines {
                let _ = master.write_all(line.as_bytes());
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if unplug {
                drop(master);
                drop(slave);
                return;
            }
            // Quiet, and plugged in, until the capture is over.
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(slave);
        });
        Self { port, thread }
    }
}

#[cfg(unix)]
fn flashed(m: &Machine) -> PathBuf {
    m.fake_espflash.as_ref().unwrap().path().join("flashed")
}

#[cfg(unix)]
#[tokio::test]
async fn a_bare_board_judges_the_serial_check_and_names_the_pin_check_unseen() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(
        flashed(&m),
        &["\u{1b}[32mHello world!\u{1b}[0m\r\n", "Hello world!\r\n"],
        false,
    );
    m.port = Some(board.port.clone());

    let (ok, v) = m.json(&server.uri(), &["run"]);
    board.thread.join().unwrap();

    assert!(ok, "{v}");
    let args = m.espflash_saw();
    assert!(
        args.contains(&format!("--port {}", board.port)) && args.contains("--chip esp32c3"),
        "{args}"
    );
    assert!(
        args.contains("--after hard-reset"),
        "ter cannot reset through a port it does not know, so espflash does: {args}"
    );
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["mode"], "hardware");
    assert_eq!(run["venue"], "local");
    assert_eq!(run["check_status"], "passed", "{run}");
    assert_eq!(
        (run["checks_seen"].as_u64(), run["checks_total"].as_u64()),
        (Some(1), Some(2)),
        "a partial pass: the site does not count it"
    );
    assert!(run["first_failure"].is_null());
    assert!(
        run["transcript_tail"]
            .as_str()
            .unwrap()
            .contains("Hello world!")
    );
    let verdicts = v["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["status"], "pass");
    assert_eq!(verdicts[1]["status"], "not_observable");

    let rec = m.exercise().join(".runs/1");
    for f in [
        "build.log",
        "flash.log",
        "serial.log",
        "events.jsonl",
        "check.yaml",
        "check.toml",
    ] {
        assert!(rec.join(f).is_file(), "{f}");
    }
    let events = std::fs::read_to_string(rec.join("events.jsonl")).unwrap();
    assert!(
        events
            .lines()
            .next()
            .unwrap()
            .contains(r#""venue":"local""#)
    );
    assert!(
        events.lines().nth(1).unwrap().contains(r#""kind":"reset""#),
        "a reset synthesised at capture start: {events}"
    );
    let again = m.ter_in(
        &server.uri(),
        &[
            "telemetry",
            "check",
            rec.to_str().unwrap(),
            "--check",
            rec.join("check.yaml").to_str().unwrap(),
        ],
        &m.exercise(),
    );
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        std::fs::read_to_string(rec.join("check.toml")).unwrap(),
        "re-checking the recording gives the same verdicts"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn text_output_shows_the_serial_and_the_partial_pass() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());

    let o = m.ter_in(&server.uri(), &["run"], &m.exercise());
    board.thread.join().unwrap();

    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        err.contains("1 of 2 checks cannot be seen here (blink-rate)"),
        "{err}"
    );
    assert!(out.contains("Ran on the board in"), "{out}");
    assert!(out.contains("  | Hello world!"), "{out}");
    assert!(out.contains("unseen  blink-rate"), "{out}");
    assert!(
        out.contains("1 of 2 checks seen on this setup, all passed. Full check: ter run --sim"),
        "{out}"
    );
}

/// The site says a pass on the learner's board did not complete the
/// lesson: one line points at the simulation run that does.
#[cfg(unix)]
#[tokio::test]
async fn a_board_pass_that_did_not_complete_the_lesson_points_at_sim() {
    let answer = ok(json!({"name": "r-0001", "attempt": 3, "lesson_completed": false}));
    let server = site("0.1.0", answer).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());

    let o = m.ter_in(&server.uri(), &["run"], &m.exercise());
    board.thread.join().unwrap();

    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("Passed on your board. Run `ter run --sim` to complete the lesson."),
        "{out}"
    );
    assert!(
        out.contains("1 of 2 checks seen on this setup, all passed.\n"),
        "the --sim pointer is said once: {out}"
    );
    assert!(!out.contains("Full check"), "{out}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_board_unplugged_mid_capture_posts_not_run_never_failed() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["booting\r\n"], true);
    m.port = Some(board.port.clone());

    let (ok, v) = m.json(&server.uri(), &["run"]);
    board.thread.join().unwrap();

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["check_status"], "not_run");
    assert!(run["first_failure"].is_null());
    let tail = run["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"), "{tail}");
    assert!(
        tail.contains("went away") && tail.contains("booting"),
        "{tail}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_flash_posts_not_run() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware"]);
    m = m.on_board(BANNER_AND_BLINK, 1, "");
    let board = Board::start(flashed(&m), &[], false);
    m.port = Some(board.port.clone());

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "flash_failed", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["check_status"], "not_run");
    let tail = run["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("flash_failed"), "{tail}");
    assert!(tail.contains("Error while connecting"), "{tail}");
    assert!(m.exercise().join(".runs/1/flash.log").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn no_board_stops_before_the_build() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-no-such-board",
    );

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("plugged in"),
        "{v}"
    );
    assert!(posted(&server, "run").await.is_empty());
    assert!(
        !m.exercise().join(".runs").exists(),
        "refused before the build"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_uf2_board_is_flashed_through_its_drive_and_heard_on_its_new_port() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    m = m.on_uf2_board(BANNER_AND_BLINK, true);
    let board = Board::start(m.uf2_copied(), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());

    let o = m.ter_in(&server.uri(), &["run"], &m.exercise());
    board.thread.join().unwrap();

    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        err.contains("then the board through its UF2 drive"),
        "{err}"
    );
    assert!(err.contains("Copying the program to"), "{err}");
    assert!(out.contains("  | Hello world!"), "{out}");
    assert!(out.contains("unseen  blink-rate"), "{out}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["venue"], "local");
    assert_eq!(run["check_status"], "passed", "{run}");
    assert_eq!(
        (run["checks_seen"].as_u64(), run["checks_total"].as_u64()),
        (Some(1), Some(2))
    );

    let rec = m.exercise().join(".runs/1");
    let uf2 = std::fs::read(rec.join("firmware.uf2")).unwrap();
    assert_eq!(
        uf2,
        std::fs::read(m.uf2_copied()).unwrap(),
        "the file copied"
    );
    assert_eq!(uf2.len(), 3 * 512, "600 bytes: three pages");
    assert_eq!(&uf2[28..32], &0xe48b_ff56u32.to_le_bytes(), "RP2040 family");
    let log = std::fs::read_to_string(rec.join("flash.log")).unwrap();
    assert!(
        log.contains("UF2 for the RP2040") && log.contains("restarted after"),
        "{log}"
    );
    let events = std::fs::read_to_string(rec.join("events.jsonl")).unwrap();
    assert!(
        events.lines().nth(1).unwrap().contains(r#""kind":"reset""#),
        "a reset synthesised at capture start: {events}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_uf2_board_not_in_its_bootloader_stops_before_the_build() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware"]).on_uf2_board(BANNER_AND_BLINK, false);
    let empty = m.uf2.as_ref().unwrap().path().join("RPI-RP2");
    std::fs::create_dir(&empty).unwrap();
    m.port = None;

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no UF2 drive there") && message.contains("hold BOOT, tap RESET"),
        "{message}"
    );
    assert!(posted(&server, "run").await.is_empty());
    assert!(
        !m.exercise().join(".runs").exists(),
        "refused before the build"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_program_the_uf2_board_cannot_take_posts_not_run() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]).on_uf2_board(BANNER_AND_BLINK, true);
    // Linked where the RP2040 has no flash.
    let wrong = ter_flash::uf2::elf32(&[(0x0002_7000, &[1; 16])]);
    std::fs::write(m.uf2.as_ref().unwrap().path().join("program.elf"), wrong).unwrap();

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "flash_failed", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["check_status"], "not_run");
    let tail = run["transcript_tail"].as_str().unwrap();
    assert!(tail.contains("outside the RP2040's flash"), "{tail}");
    assert!(!m.uf2_copied().exists(), "nothing copied");
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

/// The files the page's editor sends: the whole scaffold, with the
/// learner's `main.rs`.
fn page_files(m: &Machine, main_rs: &str) -> Value {
    let dir = m.exercise();
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
    json!([
        {"path": "Cargo.toml", "content": read("Cargo.toml")},
        {"path": "check.yaml", "content": "timeout_ms: 100\nassert: []\n"},
        {"path": "src/main.rs", "content": main_rs},
    ])
}

#[cfg(unix)]
#[tokio::test]
async fn serve_runs_the_pages_files_and_answers_with_the_posted_run() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(BROKEN, &["hardware", "simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let check = std::fs::read_to_string(m.exercise().join("check.yaml")).unwrap();
    let served = m.serve(&server.uri());

    let health = reqwest::Client::new()
        .get(format!("{}/health", served.url))
        .header("origin", PAGE)
        .send()
        .await
        .unwrap();
    assert_eq!(
        health.headers()["access-control-allow-origin"],
        PAGE,
        "the page can read it"
    );
    let health: Value = health.json().await.unwrap();
    assert_eq!(health["service"], "ter");
    assert_eq!(health["posts_to_site"], true);

    let (status, headers, v) = served
        .run(json!({"exercise_id": EXERCISE, "files": page_files(&m, GOOD)}))
        .await;

    assert_eq!(status, 200, "{v}");
    assert_eq!(headers["access-control-allow-origin"], PAGE);
    assert_eq!(v["name"], "r-0001");
    assert_eq!(v["site"]["attempt"], 3);
    assert_eq!(v["mode"], "simulation", "the editor is the simulation path");
    assert_eq!(v["check_status"], "passed", "{v}");
    assert_eq!(
        v["not_written"],
        json!(["check.yaml"]),
        "the page cannot change the check"
    );
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        GOOD
    );
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("check.yaml")).unwrap(),
        check
    );
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["venue"], "wokwi");
    assert_eq!(p["check_status"], "passed");
}

#[cfg(unix)]
#[tokio::test]
async fn serve_answers_a_failed_build_with_the_posted_run() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let served = m.serve(&server.uri());

    let (status, _, v) = served
        .run(json!({"exercise_id": EXERCISE, "files": page_files(&m, BROKEN)}))
        .await;

    assert_eq!(status, 200, "a failed build is still a posted run: {v}");
    assert_eq!(v["build_status"], "failed");
    assert!(v["compiler_tail"].as_str().unwrap().contains("led"), "{v}");
    assert_eq!(v["error"]["code"], "build_failed");
    assert_eq!(posted(&server, "run").await.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn serve_refuses_a_page_signed_in_as_someone_else() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]);
    let served = m.serve(&server.uri());

    let (status, headers, v) = served
        .run(json!({
            "exercise_id": EXERCISE,
            "files": page_files(&m, BROKEN),
            "user": "someone-else@example.com",
        }))
        .await;

    assert_eq!(status, 403);
    assert_eq!(v["error"]["code"], "wrong_account", "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains(USER));
    assert_eq!(headers["access-control-allow-origin"], PAGE);
    assert!(posted(&server, "run").await.is_empty());
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        GOOD,
        "nothing written"
    );

    // The same account is fine.
    let (status, _, v) = served
        .run(json!({"exercise_id": EXERCISE, "files": [], "user": USER}))
        .await;
    assert_eq!(status, 200, "{v}");
}

#[tokio::test]
async fn serve_refuses_any_other_origin() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]);
    let served = m.serve(&server.uri());

    let r = reqwest::Client::new()
        .post(format!("{}/run", served.url))
        .header("origin", "https://evil.example")
        .json(&json!({"exercise_id": EXERCISE, "files": page_files(&m, BROKEN)}))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status().as_u16(), 403);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    assert!(posted(&server, "run").await.is_empty());
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        GOOD
    );
}

/// A port nothing listens on yet.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const BENCH: &str = "b-0001";
const CODE: &str = "GZTS-2520";

/// The site's bench registry, for a bench whose socket is on `bench_port`,
/// heartbeating every `every` seconds.
async fn bench_site(bench_port: u16, every: u64) -> MockServer {
    let server = site("0.1.0", run_answer()).await;
    let url = format!("http://127.0.0.1:{bench_port}");
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_register")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(
            json!({"ok": true, "bench": BENCH, "board": "xiao-esp32c3",
            "label": "Desk", "heartbeat_every": every}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_share")))
        .respond_with(ok(json!({"ok": true, "bench": BENCH, "share_code": CODE})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_unshare")))
        .respond_with(ok(json!({"ok": true, "bench": BENCH, "dropped": 1})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_heartbeat")))
        .respond_with(ok(heartbeat(true)))
        .mount(&server)
        .await;
    let connected = json!({"ok": true, "bench": BENCH, "label": "Desk", "board": "xiao-esp32c3",
        "owner_name": "Owner", "status": "available", "url": url});
    for f in ["bench_connect", "bench_reconnect"] {
        Mock::given(method("POST"))
            .and(path(format!("{API}.{f}")))
            .respond_with(ok(connected.clone()))
            .mount(&server)
            .await;
    }
    server
}

fn heartbeat(sharing: bool) -> Value {
    json!({"ok": true, "bench": BENCH, "status": "available", "sharing": sharing,
           "share_code": if sharing { json!(CODE) } else { json!(null) },
           "connected": if sharing { json!(["Driver"]) } else { json!([]) }})
}

/// The owner's machine: a board on a pty, flashed by a fake espflash.
#[cfg(unix)]
fn owner_with_board(lines: &'static [&'static str]) -> (Machine, Board) {
    let mut owner = Machine::with_exercise(GOOD, &["hardware"]).on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&owner), lines, false);
    owner.port = Some(board.port.clone());
    (owner, board)
}

#[cfg(unix)]
#[tokio::test]
async fn a_run_on_a_shared_bench_is_flashed_there_and_posted_by_the_driver() {
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    let (owner, board) = owner_with_board(&["Hello world!\r\n"]);
    let served = owner.serve_with(
        &server.uri(),
        &[
            "--share",
            "--board",
            "xiao-esp32c3",
            "--label",
            "Desk",
            "--bench-port",
            &bench_port.to_string(),
        ],
    );
    assert!(
        served.banner.contains(&format!("is shared as {CODE}")),
        "{}",
        served.banner
    );
    let register = &posted(&server, "bench_register").await[0];
    assert_eq!(register["board"], "xiao-esp32c3");
    assert_eq!(register["label"], "Desk");
    assert_eq!(register["url"], format!("http://127.0.0.1:{bench_port}"));
    assert_eq!(posted(&server, "bench_share").await[0]["bench"], BENCH);

    let driver = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    let (ok, v) = driver.json_in(
        &server.uri(),
        &["connect", "gzts 2520"],
        driver.courses.path(),
    );
    assert!(ok, "{v}");
    assert_eq!(posted(&server, "bench_connect").await[0]["code"], CODE);

    let (ok, v) = driver.json(&server.uri(), &["run", "--hw", "--venue", "bench"]);
    board.thread.join().unwrap();
    assert!(ok, "{v}");

    let flashed_with = owner.espflash_saw();
    assert!(
        flashed_with.contains("--chip esp32c3")
            && flashed_with.contains(&format!("--port {}", board.port)),
        "the owner's espflash flashed the owner's board: {flashed_with}"
    );
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["mode"], "hardware");
    assert_eq!(run["venue"], "bench");
    assert_eq!(run["check_status"], "passed", "{run}");
    assert_eq!(
        (run["checks_seen"].as_u64(), run["checks_total"].as_u64()),
        (Some(1), Some(2))
    );
    let rec = driver.exercise().join(".runs/1");
    for f in [
        "build.log",
        "flash.log",
        "serial.log",
        "events.jsonl",
        "check.toml",
    ] {
        assert!(rec.join(f).is_file(), "{f} is in the driver's recording");
    }
    let events = std::fs::read_to_string(rec.join("events.jsonl")).unwrap();
    assert!(
        events
            .lines()
            .next()
            .unwrap()
            .contains(r#""venue":"bench""#),
        "{events}"
    );
    assert!(
        !owner.exercise().join(".runs").exists(),
        "nothing is recorded on the owner's side"
    );
    drop(served);
}

#[cfg(unix)]
#[tokio::test]
async fn the_kill_switch_stops_sharing_and_the_next_run_is_refused_before_building() {
    use std::io::Write;
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    let (owner, _board) = owner_with_board(&[]);
    let mut served = owner.serve_with(
        &server.uri(),
        &[
            "--share",
            "--board",
            "xiao-esp32c3",
            "--bench-port",
            &bench_port.to_string(),
        ],
    );
    let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    let (ok, v) = driver.json_in(&server.uri(), &["connect", CODE], driver.courses.path());
    assert!(ok, "{v}");

    let stdin = served.stdin.as_mut().unwrap();
    stdin.write_all(b"k\n").unwrap();
    stdin.flush().unwrap();
    let t = std::time::Instant::now();
    while posted(&server, "bench_unshare").await.is_empty() {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "no unshare"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(posted(&server, "bench_unshare").await[0]["bench"], BENCH);

    let (ok, v) = driver.json(&server.uri(), &["run", "--hw", "--venue", "bench"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not sharing"),
        "{v}"
    );
    assert!(posted(&server, "run").await.is_empty());
    assert!(
        !driver.exercise().join(".runs").exists(),
        "stopped before the build"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sharing_turned_off_on_the_site_closes_the_bench_within_a_heartbeat() {
    let bench_port = free_port();
    let server = bench_site(bench_port, 1).await;
    let (owner, _board) = owner_with_board(&[]);
    let _served = owner.serve_with(
        &server.uri(),
        &[
            "--share",
            "--board",
            "xiao-esp32c3",
            "--bench-port",
            &bench_port.to_string(),
        ],
    );
    let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    let (connected, v) = driver.json_in(&server.uri(), &["connect", CODE], driver.courses.path());
    assert!(connected, "{v}");

    // Unshared on the Devices page: the next heartbeat says so.
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_heartbeat")))
        .respond_with(ok(heartbeat(false)))
        .with_priority(1)
        .mount(&server)
        .await;
    let heartbeats = posted(&server, "bench_heartbeat").await.len();
    let t = std::time::Instant::now();
    while posted(&server, "bench_heartbeat").await.len() < heartbeats + 2 {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "no heartbeat"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        posted(&server, "bench_heartbeat").await[0]["status"],
        "available"
    );

    let (ok, v) = driver.json(&server.uri(), &["run", "--hw", "--venue", "bench"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(posted(&server, "run").await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_bench_that_does_not_answer_is_offline_and_nothing_is_posted() {
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    let (ok, v) = driver.json_in(&server.uri(), &["connect", CODE], driver.courses.path());
    assert!(ok, "{v}");

    let (ok, v) = driver.json(&server.uri(), &["run", "--hw", "--venue", "bench"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "bench_offline", "{v}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!driver.exercise().join(".runs").exists());
}

#[tokio::test]
async fn a_run_on_a_bench_needs_a_connection_first() {
    let server = site("0.1.0", run_answer()).await;
    let driver = Machine::with_exercise(GOOD, &["hardware"]);
    std::fs::write(driver.exercise().join("check.yaml"), BANNER_AND_BLINK).unwrap();
    std::fs::create_dir_all(driver.exercise().join(".cargo")).unwrap();
    std::fs::write(
        driver.exercise().join(".cargo/config.toml"),
        "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n",
    )
    .unwrap();
    let (ok, v) = driver.json(&server.uri(), &["run", "--hw", "--venue", "bench"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ter connect"),
        "{v}"
    );
}
