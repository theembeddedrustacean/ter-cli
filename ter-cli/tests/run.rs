//! `ter run`, `ter hint` and `ter status` against a local mock of the site,
//! building a small host program in a temporary courses root.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

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
/// 42, its code for a run that reached `--timeout`. Any other exit code in
/// `exit-code` fails the run early with `stderr`'s text, as wokwi-cli does
/// when Wokwi refuses the token or the minutes are used up. It keeps what
/// it was given, for the test to look at.
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
[ ! -f "$dir/wokwi.toml" ] || cp "$dir/wokwi.toml" "$here/got-wokwi.toml"
for f in "$dir"/*.chip.*; do [ ! -f "$f" ] || cp "$f" "$here/got-$(basename "$f")"; done
cp "$scenario" "$here/got-scenario.yaml"
echo "$timeout" > "$here/got-timeout"
code="$(cat "$here/exit-code")"
if [ "$code" != 42 ]; then
  cat "$here/stderr" >&2
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

/// Plays the Python that holds the sim86 package, running ter's helper:
/// `python <helper> version|balance|view <run>|run <request>|live <request>`. It answers with the
/// JSON beside it (`balance.json`, `run.json`: what the real helper
/// printed, recorded) and keeps what it was given for the test to look at.
/// Its package has a key of its own once `has-key` is beside it.
const FAKE_SIM86_PYTHON: &str = r#"#!/bin/sh
here="$(dirname "$0")"
[ -f "$1" ] || { echo "no helper at $1" >&2; exit 2; }
cp "$1" "$here/got-helper.py"
case "$2" in
  version)
    key=false; [ -f "$here/has-key" ] && key=true
    echo "{\"helper\": 4, \"ok\": true, \"python\": \"3.12.3\", \"sim86\": \"0.1.4\", \"key\": $key}" ;;
  balance) cat "$here/balance.json" ;;
  run)
    [ -f "$3" ] || { echo "no request at $3" >&2; exit 2; }
    cp "$3" "$here/got-request.json"
    echo "$SIM86_TELEMETRY" > "$here/got-telemetry"
    echo "sim86: waiting" >&2
    cat "$here/run.json" ;;
  live)
    # The run starts, and a second later does what live.jsonl says. Then
    # it runs until ter says stop or goes away, unless live-ends has it
    # end by itself.
    [ -f "$3" ] || { echo "no request at $3" >&2; exit 2; }
    cp "$3" "$here/got-live-request.json"
    echo started >> "$here/live-starts"
    if [ -f "$here/live-fails" ]; then cat "$here/live-fails"; exit 0; fi
    if [ -f "$here/live-started" ]; then cat "$here/live-started"
    else echo '{"ev": "started", "run": "sim86-run-1"}'; fi
    sleep 1
    cat "$here/live.jsonl"
    if [ -f "$here/live-ends" ]; then
      cat "$here/live-ends"
    else
      read -r word
      echo "${word:-gone}" >> "$here/got-stop"
      echo '{"ev": "ended", "reason": "stopped"}'
    fi
    echo '{"helper": 4, "ok": true, "run": "sim86-run-1"}' ;;
  view)
    echo "{\"helper\": 4, \"ok\": true, \"view\": \"https://app.sim86.com/live/$3#key=$SIM86_API_KEY\"}" ;;
esac
"#;

/// The pilot's sim86 boards: uFerris with an LED on D1 and a button on D3,
/// and a Nucleo with an LED on D13.
const SIM86_UFERRIS: &str = r#"{"name": "ter-uferris-c3-basic",
 "parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"},
           {"id": "led1", "type": "Led", "config": {"color": "red"}},
           {"id": "btn1", "type": "Btn"}],
 "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"], ["btn1.L1", "mcu.D3"], ["btn1.L2", "mcu.GND"]],
 "limits": {"duration": "30s"}}
"#;
const SIM86_UFERRIS_PINS: &str =
    r#"{"user_led": "mcu.D1", "buzzer": "mcu.D2", "user_button": {"part": "btn1"}}"#;
const SIM86_NUCLEO: &str = r#"{"name": "ter-nucleo-f401re",
 "parts": [{"id": "mcu", "type": "NucleoF401RE", "firmware": "fw.elf"},
           {"id": "probe", "type": "Led", "config": {"color": "green"}}],
 "wires": [["mcu.D13", "probe.+"], ["probe.-", "mcu.GND"]]}
"#;
const SIM86_NUCLEO_PINS: &str =
    r#"{"user_led": "mcu.D13", "user_button": {"part": "mcu", "input": "button"}}"#;
const SIM86_BALANCE: &str = r#"{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4", "balance": {"credits": 20184, "plan": "free"}}"#;
const SIM86_KEY: &str = "sk-sim86-never-shown";

/// gpio-blinky's check, and a line the chip's ROM prints at reset.
const SIM86_HEARTBEAT_CHECK: &str = "\
timeout_ms: 5500
assert:
  - id: boot
    serial_contains: \"ESP-ROM:esp32c3\"
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
    /// ter's tools folder (`TER_TOOLS_DIR`): empty, or with a stand-in
    /// for the Python that holds sim86.
    tools: TempDir,
    /// Whether sim86 is installed there, and so a key is set.
    sim86: bool,
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
            tools: tempfile::tempdir().unwrap(),
            sim86: false,
        };
        let dir = m.exercise();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"exercise\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), main_rs).unwrap();
        // An exercise that allows simulation is fetched with its circuit.
        if modes.contains(&"simulation") {
            std::fs::write(dir.join("diagram.json"), UFERRIS).unwrap();
        }
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
        let mut args = vec!["serve", "--port", "0"];
        args.extend_from_slice(extra);
        let cmd = self.command(site, &args, self.courses.path());
        self.serve_from(cmd, site, extra)
    }

    /// `cmd` is a `ter serve` of this machine, started with `extra`.
    fn serve_from(&self, mut cmd: Command, site: &str, extra: &[&str]) -> Served {
        use std::io::BufRead;
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
        // The mock site is not the live one, so the banner says which page
        // is answered: one line more than a learner sees.
        let lines = if extra.contains(&"--share") { 4 } else { 2 };
        for _ in 0..lines {
            let mut more = String::new();
            stdout.read_line(&mut more).unwrap();
            banner.push_str(&more);
        }
        Served {
            stdin: child.stdin.take(),
            child,
            url,
            page: site.trim_end_matches('/').to_string(),
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
            .env_remove("SIM86_API_KEY")
            .env("TER_TOOLS_DIR", self.tools.path())
            .env_remove("TER_PORT")
            .env_remove("TER_UF2_DRIVE")
            // A page's bench run waits this long for a busy bench.
            .env("TER_BENCH_QUEUE_SECS", "3");
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
        if self.sim86 {
            cmd.env("SIM86_API_KEY", SIM86_KEY);
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
        std::fs::write(
            fake.path().join("stderr"),
            "Error: the simulation crashed\n",
        )
        .unwrap();
        self.fake_wokwi = Some(fake);
        self
    }

    /// Where the stand-in for sim86's Python and its files are.
    fn sim86_bin(&self) -> PathBuf {
        self.tools.path().join("sim86/venv/bin")
    }

    /// Give the exercise a sim86 board (`board`, `pins`) and `check` as
    /// its check.yaml, and install a stand-in for sim86 that answers a run
    /// with `answer`: a file in tests/fixtures/sim86, or the text itself.
    #[cfg(unix)]
    fn on_sim86(mut self, check: &str, board: &str, pins: &str, answer: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.exercise();
        std::fs::write(dir.join("check.yaml"), check).unwrap();
        std::fs::create_dir_all(dir.join("sim86")).unwrap();
        std::fs::write(dir.join("sim86/board.json"), board).unwrap();
        std::fs::write(dir.join("sim86/pins.json"), pins).unwrap();

        let bin = self.sim86_bin();
        std::fs::create_dir_all(&bin).unwrap();
        let python = bin.join("python");
        std::fs::write(&python, FAKE_SIM86_PYTHON).unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(bin.join("balance.json"), SIM86_BALANCE).unwrap();
        let answer = if answer.ends_with(".json") {
            sim86_fixture(answer)
        } else {
            answer.to_string()
        };
        std::fs::write(bin.join("run.json"), answer).unwrap();
        self.sim86 = true;
        self
    }

    /// Have a real Python with a stand-in sim86 package
    /// (tests/fixtures/sim86/package) run ter's own helper, in place of
    /// the stand-in Python; files beside it in [`Self::sim86_bin`] steer
    /// the package and say what it was asked.
    #[cfg(unix)]
    fn with_sim86_package(self) -> Self {
        let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sim86/package");
        std::fs::write(
            self.sim86_bin().join("python"),
            format!(
                "#!/bin/sh\nFAKE_SIM86_DIR=\"$(dirname \"$0\")\" PYTHONPATH='{}' exec python3 \"$@\"\n",
                package.display()
            ),
        )
        .unwrap();
        self
    }

    fn sim86_saw(&self, file: &str) -> String {
        std::fs::read_to_string(self.sim86_bin().join(file)).unwrap()
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
    /// The origin of the lesson page it answers: the mock site's.
    page: String,
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
            .header("origin", &self.page)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        (status, headers, r.json().await.unwrap())
    }
}

/// The live site's lesson page, which a ter set to the mock site refuses.
const LIVE_PAGE: &str = "https://learn.theembeddedrustacean.com";

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
    // A site without bench_using answers as Frappe does for a function it
    // lacks; tests that need it mount their own with a higher priority.
    Mock::given(method("GET"))
        .and(path(format!("{API}.bench_using")))
        .respond_with(ResponseTemplate::new(417).set_body_json(json!({
            "exc_type": "ValidationError",
            "_server_messages": json!([json!({"message":
                "Failed to get method for command ter_courses.api.bench_using with module 'ter_courses.api' has no attribute 'bench_using'"
            }).to_string()]).to_string()
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
async fn a_run_rewrites_the_sites_files_that_changed_and_keeps_the_learners_code() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    let dir = m.exercise();
    std::fs::create_dir_all(dir.join("sim86")).unwrap();
    std::fs::write(dir.join("sim86/pins.json"), "{\"user_led\": \"mcu.D1\"}\n").unwrap();
    let cargo_toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let file =
        |path: &str, content: &str| json!({"path": path, "content": content, "encoding": "utf8"});
    Mock::given(method("GET"))
        .and(path(format!("{API}.exercise")))
        .respond_with(ok(json!({
            "exercise_id": EXERCISE, "title": "Heartbeat", "target": "xiao-esp32c3-nostd",
            "modes": ["hardware", "simulation"], "pins": {"user_led": "GPIO10"},
            "files": [
                file("Cargo.toml", &cargo_toml),
                file(".cargo/config.toml", "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor\"\n"),
                file("src/main.rs", "fn main() {}\n"),
                file("diagram.json", UFERRIS),
                file("sim86/pins.json", "{\"user_led\": \"mcu.D10\"}\n"),
            ]
        })))
        .mount(&server)
        .await;

    let o = m.ter_in(&server.uri(), &["run", "--no-check", "--sim"], &dir);

    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(
        err.contains("Updated from the site: .cargo/config.toml, sim86/pins.json, ter.toml.\n"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("sim86/pins.json")).unwrap(),
        "{\"user_led\": \"mcu.D10\"}\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/main.rs")).unwrap(),
        GOOD
    );
    let ter = std::fs::read_to_string(dir.join("ter.toml")).unwrap();
    assert!(ter.contains("user_led = \"GPIO10\""), "{ter}");
    assert!(ter.contains("mode = \"hardware\""), "{ter}");
    assert_eq!(posted(&server, "run").await.len(), 1);

    // Nothing changed since: nothing to say.
    let o = m.ter_in(&server.uri(), &["run", "--no-check", "--sim"], &dir);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{err}");
    assert!(!err.contains("Updated from the site"), "{err}");
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
        err.contains(
            "This run on your board can't complete the exercise: blink-rate is graded only in simulation (1 of 2 checks). \
             Your board still shows it working; `ter run --sim` completes it."
        ),
        "{err}"
    );
    assert!(out.contains("Ran on the board in"), "{out}");
    assert!(out.contains("  | Hello world!"), "{out}");
    assert!(out.contains("not graded  blink-rate"), "{out}");
    assert!(
        out.contains(
            "1 of 2 checks passed here; blink-rate is graded only in simulation, \
             so this run can't complete the exercise. To complete it: ter run --sim"
        ),
        "{out}"
    );
}

/// Modes that allow simulation, fetched with no circuit: nothing points
/// at a simulation run, and asking for one is refused before the build.
#[cfg(unix)]
#[tokio::test]
async fn an_exercise_with_no_circuit_is_not_sent_to_simulation() {
    let answer = ok(json!({"name": "r-0001", "attempt": 3, "lesson_completed": false}));
    let server = site("0.1.0", answer).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware", "simulation"]);
    std::fs::remove_file(m.exercise().join("diagram.json")).unwrap();
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());

    let o = m.ter_in(&server.uri(), &["run", "--hw"], &m.exercise());
    board.thread.join().unwrap();

    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{out}\n{err}");
    assert!(
        err.contains("This run on your board can't complete the exercise"),
        "{err}"
    );
    assert!(
        !err.contains("--sim") && !out.contains("--sim"),
        "{out}\n{err}"
    );
    assert!(!out.contains("in simulation"), "{out}");
    assert!(out.contains("lesson page"), "{out}");

    let runs = std::fs::read_dir(m.exercise().join(".runs"))
        .unwrap()
        .count();
    let (ok, v) = m.json(&server.uri(), &["run", "--sim"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_observable", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("has no Wokwi circuit (diagram.json is not in its folder)"),
        "{msg}"
    );
    assert!(msg.contains("`ter run --hw`"), "{msg}");
    assert_eq!(
        std::fs::read_dir(m.exercise().join(".runs"))
            .unwrap()
            .count(),
        runs,
        "refused before the build"
    );
    assert_eq!(posted(&server, "run").await.len(), 1, "only the board run");
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
        out.contains("so this run can't complete the exercise.\n"),
        "the --sim pointer is said once: {out}"
    );
    assert!(!out.contains("To complete it"), "{out}");
}

/// Hardware only: the lesson page completes the lesson, not --sim.
#[cfg(unix)]
#[tokio::test]
async fn a_hardware_only_board_pass_points_at_the_lesson_page() {
    let answer = ok(json!({"name": "r-0001", "attempt": 3, "lesson_completed": false}));
    let server = site("0.1.0", answer).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());

    let o = m.ter_in(&server.uri(), &["run"], &m.exercise());
    board.thread.join().unwrap();

    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains(
            "Passed on your board, but some checks are graded only with a telemetry board. \
             Mark the lesson complete on the lesson page to continue."
        ),
        "{out}"
    );
    assert!(!out.contains("--sim"), "{out}");
    assert!(
        out.contains(
            "blink-rate is graded only with a telemetry board, which ter does not support yet"
        ),
        "{out}"
    );
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
    assert!(out.contains("not graded  blink-rate"), "{out}");
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

    // The answer carries what was recorded, in time order, each pin with
    // the board's name for it; the site is not sent it.
    let events = v["events"].as_array().expect("events");
    assert_eq!(events[0], json!({"t_us": 0, "kind": "reset"}), "{v}");
    let edges: Vec<&Value> = events.iter().filter(|e| e["kind"] == "pin_edge").collect();
    assert!(edges.len() > 16, "{v}");
    for e in &edges {
        assert_eq!(
            (&e["name"], &e["pin"]),
            (&json!("user_led"), &json!("GPIO3"))
        );
        assert!(e.get("net").is_none(), "Wokwi has no names of its own: {e}");
    }
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "serial" && e["text"].as_str().unwrap().contains("Hello world!")),
        "{v}"
    );
    let times: Vec<u64> = events.iter().map(|e| e["t_us"].as_u64().unwrap()).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    assert_eq!(v["events_end_us"], 5_500_000, "{v}");
    assert!(v.get("events_cut").is_none(), "{v}");
    assert!(p.get("events").is_none(), "{p}");
    let recorded = std::fs::read_to_string(m.exercise().join(".runs/1/events.jsonl")).unwrap();
    assert_eq!(
        recorded.lines().count(),
        events.len() + 1,
        "the header and every event"
    );

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
async fn a_custom_chip_goes_into_the_run_folder_with_its_wokwi_toml_entry() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let dir = m.exercise();
    let mut diagram: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("diagram.json")).unwrap()).unwrap();
    diagram["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "chip-fake", "id": "io1", "attrs": {}}));
    std::fs::write(dir.join("diagram.json"), diagram.to_string()).unwrap();
    // As the site ships one: the .wasm decoded from base64, every byte
    // value in it, the .json beside it, and the scaffold's wokwi.toml.
    let wasm: Vec<u8> = (0..=255u8).rev().cycle().take(1000).collect();
    std::fs::write(dir.join("fake.chip.wasm"), &wasm).unwrap();
    std::fs::write(
        dir.join("fake.chip.json"),
        r#"{"name":"fake","pins":["A"]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wokwi.toml"),
        "[wokwi]\nversion = 1\nfirmware = \"target/riscv32imc-unknown-none-elf/debug/exercise\"\n\n[[chip]]\nname = \"fake\"\nbinary = \"fake.chip.wasm\"\n",
    )
    .unwrap();

    let (ok, v) = m.json(&server.uri(), &["run", "--sim"]);

    assert!(ok, "{v}");
    assert_eq!(
        m.fake_saw("got-wokwi.toml"),
        "[wokwi]\nversion = 1\nfirmware = \"firmware.elf\"\nelf = \"firmware.elf\"\n\
         \n[[chip]]\nname = \"fake\"\nbinary = \"fake.chip.wasm\"\n"
    );
    let fake = m.fake_wokwi.as_ref().unwrap().path();
    assert_eq!(
        std::fs::read(fake.join("got-fake.chip.wasm")).unwrap(),
        wasm
    );
    assert_eq!(
        m.fake_saw("got-fake.chip.json"),
        r#"{"name":"fake","pins":["A"]}"#
    );
    // The run keeps its wokwi.toml; the chip copies go, as the program does.
    let run = dir.join(".runs/1");
    assert!(run.join("wokwi.toml").is_file());
    assert!(!run.join("fake.chip.wasm").exists() && !run.join("fake.chip.json").exists());
    assert_eq!(std::fs::read(dir.join("fake.chip.wasm")).unwrap(), wasm);
}

#[cfg(unix)]
#[tokio::test]
async fn a_circuit_without_its_chip_is_refused_before_the_build() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let dir = m.exercise();
    let diagram = std::fs::read_to_string(dir.join("diagram.json"))
        .unwrap()
        .replacen(
            "\"parts\": [",
            "\"parts\": [{\"type\": \"chip-fake\", \"id\": \"io1\"},",
            1,
        );
    std::fs::write(dir.join("diagram.json"), diagram).unwrap();

    let (ok, v) = m.json(&server.uri(), &["run", "--sim"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_observable", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("custom chip fake"),
        "{v}"
    );
    assert!(!dir.join(".runs").exists(), "nothing built, nothing posted");
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

/// A simulated run whose fake `wokwi-cli` stops with exit code 1 and
/// prints `stderr`, as wokwi-cli does when Wokwi turns the account away.
#[cfg(unix)]
async fn wokwi_fails(stderr: &str) -> (serde_json::Value, serde_json::Value) {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        1,
    );
    std::fs::write(m.fake_wokwi.as_ref().unwrap().path().join("stderr"), stderr).unwrap();

    let (ok, v) = m.json(&server.uri(), &["run"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let p = posted(&server, "run").await[0]["payload"].clone();
    assert_eq!(p["check_status"], "not_run", "never failed");
    assert!(p.get("first_failure").is_none());
    let tail = p["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"));
    assert!(
        tail.contains(stderr.trim()),
        "wokwi-cli's own words kept: {tail}"
    );
    (v, p)
}

#[cfg(unix)]
#[tokio::test]
async fn used_up_wokwi_minutes_say_so_and_are_posted_as_not_run() {
    let (v, p) =
        wokwi_fails("API Error: You have used up your Free plan monthly CI minute quota\n").await;

    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("used up its simulation minutes"), "{msg}");
    assert!(msg.contains("https://wokwi.com/pricing"), "{msg}");
    assert!(msg.contains("posted as not run"), "{msg}");
    assert!(!msg.contains("token"), "{msg}");
    let tail = p["transcript_tail"].as_str().unwrap();
    assert!(tail.contains("used up its simulation minutes"), "{tail}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_refused_wokwi_token_says_so_and_is_posted_as_not_run() {
    let (v, p) = wokwi_fails("API Error: Unauthorized\n").await;

    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("refused your token"), "{msg}");
    assert!(msg.contains("https://wokwi.com/dashboard/ci"), "{msg}");
    assert!(msg.contains("ter install wokwi-cli"), "{msg}");
    assert!(!msg.contains("pricing"), "{msg}");
    assert!(
        !msg.contains("fake-wokwi"),
        "the token itself never shows: {msg}"
    );
    let tail = p["transcript_tail"].as_str().unwrap();
    assert!(tail.contains("refused your token"), "{tail}");
    assert!(!tail.contains("fake-wokwi"), "{tail}");
}

#[cfg(unix)]
#[tokio::test]
async fn any_other_wokwi_failure_quotes_wokwi_cli() {
    let (v, _) = wokwi_fails("Error: the simulation crashed\n").await;

    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("exit code 1: Error: the simulation crashed"),
        "{msg}"
    );
    assert!(!msg.contains("token") && !msg.contains("minutes"), "{msg}");
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
        .header("origin", &served.page)
        .send()
        .await
        .unwrap();
    assert_eq!(
        health.headers()["access-control-allow-origin"],
        served.page.as_str(),
        "the page can read it"
    );
    let health: Value = health.json().await.unwrap();
    assert_eq!(health["service"], "ter");
    assert_eq!(health["posts_to_site"], true);

    let (status, headers, v) = served
        .run(json!({"exercise_id": EXERCISE, "files": page_files(&m, GOOD)}))
        .await;

    assert_eq!(status, 200, "{v}");
    assert_eq!(headers["access-control-allow-origin"], served.page.as_str());
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

/// A build script that, while `$SLOW_BUILD_DIR/slow` exists, starts a
/// long `sleep`, notes its own, its parent's (cargo's) and the sleep's
/// process ids in `$SLOW_BUILD_DIR/building`, and waits for the sleep.
const SLOW_BUILD_RS: &str = r#"
fn main() {
    let Ok(dir) = std::env::var("SLOW_BUILD_DIR") else { return };
    let dir = std::path::Path::new(&dir);
    // Not there after a fast build: cargo then runs this every build.
    println!("cargo:rerun-if-changed={}", dir.join("slow").display());
    if !dir.join("slow").exists() {
        return;
    }
    let mut sleep = std::process::Command::new("sleep").arg("120").spawn().unwrap();
    let ids = format!("{} {} {}", std::os::unix::process::parent_id(), std::process::id(), sleep.id());
    std::fs::write(dir.join("building.tmp"), ids).unwrap();
    std::fs::rename(dir.join("building.tmp"), dir.join("building")).unwrap();
    sleep.wait().unwrap();
}
"#;

/// Whether process `pid` still runs (a zombie nobody reaped does not).
#[cfg(target_os = "linux")]
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

/// The process ids a slow build noted, once it is building.
#[cfg(target_os = "linux")]
async fn slow_build_started(marks: &Path) -> Vec<String> {
    for _ in 0..1800 {
        if let Ok(ids) = std::fs::read_to_string(marks.join("building")) {
            std::fs::remove_file(marks.join("building")).unwrap();
            return ids.split_whitespace().map(str::to_string).collect();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the slow build never started");
}

/// Wait up to 10 s for every one of `ids` to be gone.
#[cfg(target_os = "linux")]
async fn all_gone(ids: &[String]) -> bool {
    for _ in 0..100 {
        if !ids.iter().any(|id| running(id)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// The page's Stop drops its `/run` while ter builds, and a double click
/// asks for a second run while the first builds: either way cargo and
/// everything it started are stopped at once, nothing is posted for that
/// run, and the next run builds and runs straight away.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn serve_stops_the_build_of_a_run_the_page_dropped_or_replaced() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    std::fs::write(m.exercise().join("build.rs"), SLOW_BUILD_RS).unwrap();
    let marks = tempfile::tempdir().unwrap();
    let served = serve_with_env(
        &m,
        &server.uri(),
        &[("SLOW_BUILD_DIR", marks.path().to_str().unwrap())],
    );
    let ask = || {
        let request = reqwest::Client::new()
            .post(format!("{}/run", served.url))
            .header("origin", &served.page)
            .json(&json!({"exercise_id": EXERCISE}));
        tokio::spawn(async move {
            let r = request.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        })
    };

    // Dropped: the page's Stop aborts the request, which closes it.
    std::fs::write(marks.path().join("slow"), "").unwrap();
    let dropped = ask();
    let ids = slow_build_started(marks.path()).await;
    assert_eq!(ids.len(), 3, "{ids:?}");
    dropped.abort();
    assert!(
        all_gone(&ids).await,
        "cargo, its build script and the sleep it started are stopped: {ids:?}"
    );

    // Replaced: a newer run while the first builds. It builds at once,
    // not after the dropped run's sleep.
    let dropped_at = std::time::Instant::now();
    let first = ask();
    let ids = slow_build_started(marks.path()).await;
    assert!(
        dropped_at.elapsed() < Duration::from_secs(60),
        "{:?}",
        dropped_at.elapsed()
    );
    std::fs::remove_file(marks.path().join("slow")).unwrap();
    let asked = std::time::Instant::now();
    let (status, _, v) = served.run(json!({"exercise_id": EXERCISE})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["check_status"], "passed", "{v}");
    assert!(
        asked.elapsed() < Duration::from_secs(90),
        "the newer run did not wait for the first's build: {:?}",
        asked.elapsed()
    );
    assert!(all_gone(&ids).await, "{ids:?}");
    let (status, v) = first.await.unwrap();
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["error"]["code"], "replaced", "{v}");
    assert_eq!(
        posted(&server, "run").await.len(),
        1,
        "only the run that ran is posted"
    );

    // Ctrl-C on ter serve while it builds stops the build too, though
    // cargo is in a process group of its own.
    std::fs::write(marks.path().join("slow"), "").unwrap();
    let _building = ask();
    let ids = slow_build_started(marks.path()).await;
    let ter = served.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &ter])
            .status()
            .unwrap()
            .success()
    );
    assert!(all_gone(&ids).await, "{ids:?}");
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
    assert_eq!(headers["access-control-allow-origin"], served.page.as_str());
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

#[tokio::test]
async fn serve_answers_the_page_of_the_site_it_is_set_to_and_not_the_live_one() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]);
    let served = m.serve(&server.uri());
    assert!(
        served.banner.contains(&format!(
            "only a lesson page on {} can start runs here",
            served.page
        )),
        "{}",
        served.banner
    );

    let health = reqwest::Client::new()
        .get(format!("{}/health", served.url))
        .header("origin", &served.page)
        .send()
        .await
        .unwrap();
    assert_eq!(health.status().as_u16(), 200);
    assert_eq!(
        health.headers()["access-control-allow-origin"],
        served.page.as_str()
    );

    let r = reqwest::Client::new()
        .post(format!("{}/run", served.url))
        .header("origin", LIVE_PAGE)
        .json(&json!({"exercise_id": EXERCISE, "files": page_files(&m, BROKEN)}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "origin_refused");
    let said = v["error"]["message"].as_str().unwrap();
    assert!(said.contains(&served.page), "{said}");
    assert!(posted(&server, "run").await.is_empty());
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        GOOD,
        "nothing written"
    );
}

#[tokio::test]
async fn serve_refuses_a_run_that_names_no_origin() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]);
    let served = m.serve(&server.uri());

    let r = reqwest::Client::new()
        .post(format!("{}/run", served.url))
        .json(&json!({"exercise_id": EXERCISE, "files": page_files(&m, BROKEN)}))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status().as_u16(), 403);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "origin_refused");
    assert!(posted(&server, "run").await.is_empty());
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        GOOD
    );
    let health = reqwest::get(format!("{}/health", served.url))
        .await
        .unwrap();
    assert_eq!(health.status().as_u16(), 200, "a health probe still works");
}

#[tokio::test]
async fn serve_trusts_the_dev_server_only_with_dev() {
    const DEV: &str = "http://localhost:8080";
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]);
    let health = |url: String| async move {
        reqwest::Client::new()
            .get(format!("{url}/health"))
            .header("origin", DEV)
            .send()
            .await
            .unwrap()
    };

    let plain = m.serve(&server.uri());
    let r = health(plain.url.clone()).await;
    assert_eq!(r.status().as_u16(), 403);
    assert!(r.headers().get("access-control-allow-origin").is_none());
    assert!(!plain.banner.contains("8080"), "{}", plain.banner);
    drop(plain);

    let dev = m.serve_with(&server.uri(), &["--dev"]);
    let r = health(dev.url.clone()).await;
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["access-control-allow-origin"], DEV);
}

#[tokio::test]
async fn only_an_esp32_c3_can_be_shared() {
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);
    for board in ["xiao-rp2040", "nucleo-f401re", "xiao-esp32s3"] {
        let out = m.ter_in(
            &server.uri(),
            &[
                "serve",
                "--port",
                "0",
                "--share",
                "--board",
                board,
                "--bench-port",
                &bench_port.to_string(),
            ],
            m.courses.path(),
        );
        assert!(!out.status.success(), "{board}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains("Only the ESP32-C3"), "{board}: {said}");
        assert!(said.contains(board), "{board}: {said}");
    }
    assert!(posted(&server, "bench_register").await.is_empty());
    assert!(posted(&server, "bench_share").await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn the_owner_is_warned_before_the_bench_is_shared() {
    use std::io::{BufRead, Read};
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    let (owner, _board) = owner_with_board(&[]);
    let port = bench_port.to_string();
    let mut cmd = owner.command(
        &server.uri(),
        &[
            "serve",
            "--port",
            "0",
            "--share",
            "--board",
            "xiao-esp32c3",
            "--bench-port",
            &port,
        ],
        owner.courses.path(),
    );
    let mut child = {
        let _spawning = SPAWN.read().unwrap();
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    // The banner comes once the bench is shared.
    let mut banner = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut banner)
        .unwrap();
    assert!(banner.starts_with("ter serve on "), "{banner}");
    assert_eq!(posted(&server, "bench_share").await.len(), 1);
    child.kill().unwrap();
    child.wait().unwrap();
    let mut said = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    assert!(said.contains("burn eFuses"), "{said}");
    assert!(said.contains("short"), "{said}");
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

#[cfg(unix)]
#[tokio::test]
async fn serve_runs_the_fetched_folder_when_the_page_sends_no_files() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let before = std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap();
    let served = m.serve(&server.uri());

    // The Run button without the editor: no files at all, then null, then
    // an empty list.
    for body in [
        json!({"exercise_id": EXERCISE}),
        json!({"exercise_id": EXERCISE, "files": null}),
        json!({"exercise_id": EXERCISE, "files": []}),
    ] {
        let (status, _, v) = served.run(body.clone()).await;
        assert_eq!(status, 200, "{body}: {v}");
        assert_eq!(v["check_status"], "passed", "{v}");
        assert!(v.get("not_written").is_none(), "{v}");
    }
    assert_eq!(
        std::fs::read_to_string(m.exercise().join("src/main.rs")).unwrap(),
        before,
        "the folder's own source ran, untouched"
    );
    assert_eq!(posted(&server, "run").await.len(), 3);
}

/// A page that names no mode and no venue, for an exercise that only
/// runs on a board: it runs on this machine's board, not in a simulation
/// the exercise does not allow.
#[cfg(unix)]
#[tokio::test]
async fn serve_runs_a_hardware_only_exercise_on_the_board_when_the_page_names_no_mode() {
    let server = site("0.1.0", run_answer()).await;
    let mut m = Machine::with_exercise(GOOD, &["hardware"]);
    m = m.on_board(BANNER_AND_BLINK, 0, "");
    let board = Board::start(flashed(&m), &["Hello world!\r\n"], false);
    m.port = Some(board.port.clone());
    let served = m.serve(&server.uri());

    let (status, _, v) = served.run(json!({"exercise_id": EXERCISE})).await;
    board.thread.join().unwrap();

    assert_eq!(status, 200, "{v}");
    assert_eq!(v["mode"], "hardware", "{v}");
    assert_eq!(v["venue"], "local", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["mode"], "hardware", "{run}");
}

/// `/health` as the lesson page reads it.
async fn health_of(served: &Served) -> Value {
    reqwest::Client::new()
        .get(format!("{}/health", served.url))
        .header("origin", &served.page)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// The page shows the exercise on a sim86 board and asks for that run on
/// sim86: it goes there though the folder has a Wokwi circuit too, and the
/// answer carries the recording for the page to play.
#[cfg(unix)]
#[tokio::test]
async fn serve_runs_on_sim86_when_the_page_names_it() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );
    assert!(m.exercise().join("diagram.json").is_file());
    let mut ter = std::fs::read_to_string(m.exercise().join("ter.toml")).unwrap();
    ter.push_str("\n[pins]\nuser_led = \"GPIO3\"\n");
    std::fs::write(m.exercise().join("ter.toml"), ter).unwrap();
    let served = m.serve(&server.uri());

    let health = health_of(&served).await;
    assert_eq!(
        health["simulators"][1],
        json!({"name": "sim86", "ready": true}),
        "{health}"
    );

    let (status, _, v) = served
        .run(json!({"exercise_id": EXERCISE, "mode": "simulation", "venue": "sim86"}))
        .await;

    assert_eq!(status, 200, "{v}");
    assert_eq!(
        (&v["mode"], &v["venue"]),
        (&json!("simulation"), &json!("sim86"))
    );
    assert_eq!(v["check_status"], "passed", "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["venue"], "sim86");
    assert!(p.get("events").is_none(), "events are not posted: {p}");

    let events = v["events"].as_array().expect("events");
    assert_eq!(events[0], json!({"t_us": 0, "kind": "reset"}));
    let edges: Vec<&Value> = events.iter().filter(|e| e["kind"] == "pin_edge").collect();
    // The LED's level at the start, then the first edge of the run.
    assert_eq!(
        edges[0],
        &json!({"t_us": 0, "kind": "pin_edge", "name": "user_led", "level": "low",
                "pin": "GPIO3", "net": "mcu.D1"})
    );
    assert_eq!(
        edges[1],
        &json!({"t_us": 143_325, "kind": "pin_edge", "name": "user_led", "level": "high",
                "pin": "GPIO3", "net": "mcu.D1"})
    );
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "serial" && e["text"].as_str().unwrap().contains("ESP-ROM")),
        "{v}"
    );
    assert_eq!(v["events_end_us"], 5_500_000);
    assert!(v.get("events_cut").is_none(), "{v}");
    assert!(!v.to_string().contains(SIM86_KEY));
}

/// What sim86 does in a live run of the heartbeat, as the helper prints
/// it: two pin changes, a serial line, then the check's window read back.
#[cfg(unix)]
fn live_blinky() -> String {
    let recorded = sim86_fixture("blinky.json");
    format!(
        "{}\n{}\n{}\n{{\"ev\": \"recording\", {}\n",
        r#"{"ev": "pin", "t_ns": 0, "net": "mcu.D1", "level": 0}"#,
        r#"{"ev": "pin", "t_ns": 143324763, "net": "mcu.D1", "level": 1}"#,
        r#"{"ev": "serial", "t_ms": 12.0, "source": "mcu:uart0", "line": "hello"}"#,
        &recorded.trim()[1..]
    )
}

/// The heartbeat exercise on a sim86 board, with a stand-in for sim86
/// whose live run prints `live`.
#[cfg(unix)]
fn live_machine(main_rs: &str, live: &str) -> Machine {
    let m = Machine::with_exercise(main_rs, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );
    let mut ter = std::fs::read_to_string(m.exercise().join("ter.toml")).unwrap();
    ter.push_str("\n[pins]\nuser_led = \"GPIO3\"\n");
    std::fs::write(m.exercise().join("ter.toml"), ter).unwrap();
    std::fs::write(m.sim86_bin().join("live.jsonl"), live).unwrap();
    m
}

/// A live run's event stream, as the lesson page reads it.
struct LiveStream {
    response: reqwest::Response,
    text: String,
}

impl LiveStream {
    async fn open(served: &Served, id: &str) -> Self {
        let response = reqwest::Client::new()
            .get(format!("{}/live/{id}/events", served.url))
            .header("origin", &served.page)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            served.page.as_str()
        );
        Self {
            response,
            text: String::new(),
        }
    }

    /// The next event (its name and its one JSON object), or `None` when
    /// the stream has closed.
    async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.text.find("\n\n") {
                let block: String = self.text.drain(..end + 2).collect();
                let field = |name: &str| {
                    block
                        .lines()
                        .find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()))
                };
                match (field("event:"), field("data:")) {
                    (Some(name), Some(data)) => {
                        return Some((name, serde_json::from_str(&data).unwrap()));
                    }
                    // A keep-alive comment.
                    _ => continue,
                }
            }
            let chunk = tokio::time::timeout(Duration::from_secs(30), self.response.chunk())
                .await
                .expect("an event within 30 s")
                .unwrap()?;
            self.text.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// The next event named `name`, and the names of those before it.
    async fn until(&mut self, name: &str) -> (Value, Vec<String>) {
        let mut before = Vec::new();
        loop {
            let (got, data) = self
                .next()
                .await
                .unwrap_or_else(|| panic!("the stream closed before {name}: {before:?}"));
            if got == name {
                return (data, before);
            }
            before.push(got);
        }
    }
}

impl Served {
    /// The status `/live/<id>/events` answers the lesson page with.
    async fn live_status(&self, id: &str) -> u16 {
        reqwest::Client::new()
            .get(format!("{}/live/{id}/events", self.url))
            .header("origin", &self.page)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// What `/live/<id>/view` answers the frame in the lesson page with:
    /// the status and where it sends the browser. A frame names no origin.
    async fn live_view(&self, id: &str) -> (u16, String) {
        let r = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("{}/live/{id}/view", self.url))
            .send()
            .await
            .unwrap();
        let to = r
            .headers()
            .get("location")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (r.status().as_u16(), to)
    }

    async fn live_post(&self, id: &str, what: &str, body: Value) -> (u16, Value) {
        let r = reqwest::Client::new()
            .post(format!("{}/live/{id}/{what}", self.url))
            .header("origin", &self.page)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }
}

fn live_ask(limit_s: u64) -> Value {
    json!({"exercise_id": EXERCISE, "mode": "simulation", "venue": "sim86",
           "live": {"limit_s": limit_s}})
}

/// A live run shows the board's other wired pins beside the checks', by
/// the chip's name for each when sim86 gives it, and judges the checks'
/// pins only.
#[cfg(unix)]
#[tokio::test]
async fn serve_shows_the_boards_other_wired_pins_live() {
    let server = site("0.1.0", run_answer()).await;
    let recorded = sim86_fixture("blinky.json");
    let live = format!(
        "{}\n{}\n{}\n{{\"ev\": \"recording\", {}\n",
        r#"{"ev": "pin", "t_ns": 0, "net": "mcu.D3", "level": 1}"#,
        r#"{"ev": "pin", "t_ns": 5, "net": "mcu.D9", "level": 1}"#,
        r#"{"ev": "pin", "t_ns": 10, "net": "mcu.D1", "level": 0}"#,
        &recorded.trim()[1..]
    );
    let m = live_machine(GOOD, &live);
    std::fs::write(
        m.sim86_bin().join("live-started"),
        r#"{"ev": "started", "run": "sim86-run-1", "signals": {"D1": "GPIO3", "D3": "GPIO5"}}
"#,
    )
    .unwrap();
    let served = m.serve(&server.uri());
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();

    let mut events = LiveStream::open(&served, &id).await;
    // No part of this board is one the page can change.
    assert_eq!(
        events.next().await.unwrap(),
        ("controls".to_string(), json!({"parts": []}))
    );
    assert_eq!(
        events.next().await.unwrap(),
        (
            "pin".to_string(),
            json!({"t_ms": 0.0, "pin": "GPIO5", "name": "D3", "net": "mcu.D3", "level": 1})
        )
    );
    // A net ter did not ask for is not sent; the checks' pin keeps its name.
    assert_eq!(
        events.next().await.unwrap(),
        (
            "pin".to_string(),
            json!({"t_ms": 1e-5, "pin": "GPIO3", "name": "user_led", "net": "mcu.D1", "level": 0})
        )
    );
    let (checks, _) = events.until("checks").await;
    assert_eq!(checks["check_status"], "passed", "{checks}");
    let runs = posted(&server, "run").await;
    assert_eq!(runs.len(), 1);
    assert!(
        !runs[0].to_string().contains("mcu.D3"),
        "a shown pin is not in the run record"
    );
    let asked: Value = serde_json::from_str(&m.sim86_saw("got-live-request.json")).unwrap();
    assert_eq!(asked["watch"], json!(["mcu.D1"]));
    assert_eq!(asked["show"], json!(["mcu.D3"]));
    assert_eq!(
        served.live_post(&id, "stop", json!({})).await,
        (200, json!({"ok": true}))
    );
    events.until("ended").await;
}

/// The page asks for a live run: `/run` answers as soon as the board runs,
/// the events come as sim86 gives them, the checks are judged from the
/// recording and posted while the board runs on, and Stop ends the run.
#[cfg(unix)]
#[tokio::test]
async fn serve_shows_a_sim86_run_live_and_stops_it_when_told() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_blinky());
    let served = m.serve(&server.uri());
    assert_eq!(health_of(&served).await["live"], true);

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["sim86_run"], "sim86-run-1", "{v}");
    let started_at = v["started_at"].as_u64().unwrap();
    assert!(started_at >= before, "{v}");
    assert_eq!(v["ends_at"].as_u64().unwrap(), started_at + 60_000);
    assert!(v.get("check_status").is_none(), "not a run record: {v}");
    assert!(
        posted(&server, "run").await.is_empty(),
        "nothing is posted yet"
    );
    let id = v["live"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32, "an id nobody can guess: {id}");

    let mut events = LiveStream::open(&served, &id).await;
    assert_eq!(events.next().await.unwrap().0, "controls");
    let (name, first) = events.next().await.unwrap();
    assert_eq!(name, "pin");
    assert_eq!(
        first,
        json!({"t_ms": 0.0, "pin": "GPIO3", "name": "user_led", "net": "mcu.D1", "level": 0})
    );

    // The frame in the page is sent on to sim86's own page for the run,
    // signed in with the learner's key, which only the browser gets.
    assert_eq!(
        served.live_view(&id).await,
        (
            302,
            format!("https://app.sim86.com/live/sim86-run-1#key={SIM86_KEY}")
        )
    );
    assert_eq!(served.live_view("0123").await.0, 404);
    let (_, edge) = events.next().await.unwrap();
    assert_eq!(
        (&edge["t_ms"], &edge["level"]),
        (&json!(143.324763), &json!(1))
    );
    assert_eq!(
        events.next().await.unwrap(),
        ("serial".to_string(), json!({"t_ms": 12.0, "line": "hello"}))
    );

    // The checks: the run record `/run` answers with for the same run.
    let (checks, _) = events.until("checks").await;
    assert_eq!(checks["check_status"], "passed", "{checks}");
    assert_eq!(checks["venue"], "sim86");
    assert!(
        checks["verdicts"].is_array() && checks["name"].is_string(),
        "{checks}"
    );
    assert_eq!(checks["events_end_us"], 5_500_000);
    let runs = posted(&server, "run").await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["payload"]["check_status"], "passed");
    // sim86 got no time limit of its own, only the stop of last resort.
    let asked: Value = serde_json::from_str(&m.sim86_saw("got-live-request.json")).unwrap();
    assert!(asked.get("duration_ms").is_none(), "{asked}");
    assert_eq!(asked["realtime_ms"], 1_260_000);
    assert_eq!(asked["watch"], json!(["mcu.D1"]));
    assert_eq!(asked["show"], json!(["mcu.D3"]), "the button's pin");

    // The run goes on after the checks, until the page stops it.
    assert!(!m.sim86_bin().join("got-stop").exists());
    assert_eq!(
        served.live_post(&id, "stop", json!({})).await,
        (200, json!({"ok": true}))
    );
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended, json!({"reason": "user-stop", "detail": ""}));
    assert_eq!(events.next().await, None, "the stream closes after ended");
    assert_eq!(m.sim86_saw("got-stop").trim(), "stop");

    // Over: a browser that reconnects the closed stream by itself gets no
    // new one, and the run cannot be extended.
    assert_eq!(served.live_status(&id).await, 404);
    assert_eq!(served.live_view(&id).await, (404, String::new()));
    let (status, v) = served.live_post(&id, "extend", json!({"add_s": 60})).await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (409, Some("ended")),
        "{v}"
    );
    assert_eq!(posted(&server, "run").await.len(), 1, "posted once");
    assert!(!v.to_string().contains(SIM86_KEY));
}

/// ter holds the limit: the run ends when its time is up, a minute at a
/// time more when the page extends it, and never past ten minutes.
#[cfg(unix)]
#[tokio::test]
async fn a_live_run_ends_at_its_limit_which_the_page_can_extend_up_to_the_cap() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_blinky());
    let served = m.serve(&server.uri());

    let (status, _, v) = served.run(live_ask(3)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let (started_at, ends_at) = (
        v["started_at"].as_u64().unwrap(),
        v["ends_at"].as_u64().unwrap(),
    );
    assert_eq!(ends_at, started_at + 3_000);
    let mut events = LiveStream::open(&served, &id).await;

    let (status, v) = served.live_post(&id, "extend", json!({"add_s": 2})).await;
    assert_eq!((status, &v), (200, &json!({"ends_at": ends_at + 2_000})));
    let (limit, _) = events.until("limit").await;
    assert_eq!(limit, json!({"ends_at": ends_at + 2_000}));
    // 5 s so far; 596 more would pass ten minutes.
    let (status, v) = served.live_post(&id, "extend", json!({"add_s": 596})).await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (409, Some("limit_cap")),
        "{v}"
    );
    let (status, v) = served
        .live_post(&id, "extend", json!({"add_s": "a minute"}))
        .await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (400, Some("bad_request")),
        "{v}"
    );
    let (status, v) = served.live_post("no-such-run", "stop", json!({})).await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );

    let (ended, before) = events.until("ended").await;
    assert_eq!(ended["reason"], "limit", "{ended}");
    assert!(before.contains(&"checks".to_string()), "{before:?}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(now >= ends_at + 2_000, "not before the extended limit");
    assert_eq!(m.sim86_saw("got-stop").trim(), "stop");
}

/// A closed tab does not keep a run going, and a new run stops the one
/// before it.
#[cfg(unix)]
#[tokio::test]
async fn a_live_run_nobody_watches_is_stopped_and_a_new_run_replaces_the_one_before() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_blinky());
    let mut cmd = m.command(&server.uri(), &["serve", "--port", "0"], m.courses.path());
    cmd.env("TER_LIVE_UNWATCHED_SECS", "2");
    let served = m.serve_from(cmd, &server.uri(), &[]);

    // Nobody listens.
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let first = v["live"].as_str().unwrap().to_string();
    let stopped = || m.sim86_bin().join("got-stop").exists();
    for _ in 0..100 {
        if stopped() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped(), "the run was left going with no listener");
    for _ in 0..50 {
        if served.live_status(&first).await == 404 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(served.live_status(&first).await, 404, "the run is over");
    // It ran long enough to be judged, so it is posted as any run.
    assert_eq!(posted(&server, "run").await.len(), 1);

    // A second run, watched; a third replaces it.
    let (_, _, v) = served.run(live_ask(60)).await;
    let second = v["live"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    let (status, v) = served.live_post(&first, "stop", json!({})).await;
    assert_eq!(status, 404, "the run before is gone: {v}");
    let mut events = LiveStream::open(&served, &second).await;
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_ne!(v["live"], json!(second));
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended["reason"], "replaced");
    let third = v["live"].as_str().unwrap().to_string();
    assert_eq!(served.live_post(&third, "stop", json!({})).await.0, 200);
}

/// `ter run --sim sim86 --live`: the board's changes and what it prints,
/// as they come, until the limit; then the verdicts, as `ter run` prints
/// them, and the same exit status.
#[cfg(unix)]
#[tokio::test]
async fn ter_run_live_prints_the_run_as_it_happens_then_the_verdicts() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_blinky());

    let o = m.ter_in(
        &server.uri(),
        &["run", "--sim", "sim86", "--live", "--limit", "3"],
        &m.exercise(),
    );

    let (out, err) = (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
    );
    assert!(o.status.success(), "{out}\n{err}");
    let at = |what: &str| {
        out.find(what)
            .unwrap_or_else(|| panic!("no {what:?} in:\n{out}"))
    };
    assert!(
        at("0.000 ms  GPIO3 low") < at("143.325 ms  GPIO3 high"),
        "{out}"
    );
    assert!(
        at("12.000 ms  | hello") < at("heartbeat"),
        "the verdicts come last: {out}"
    );
    assert!(out.contains("Posted attempt"), "{out}");
    assert!(err.contains("The run ended (limit)"), "{err}");
    assert_eq!(m.sim86_saw("got-stop").trim(), "stop");
    assert_eq!(posted(&server, "run").await.len(), 1);

    // Its time is up before the check's is: nothing is scored or posted.
    std::fs::write(m.sim86_bin().join("live.jsonl"), live_before_the_checks()).unwrap();
    let o = m.ter_in(
        &server.uri(),
        &["run", "--sim", "sim86", "--live", "--limit", "2"],
        &m.exercise(),
    );
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}");
    assert!(
        out.contains("GPIO3 high") && out.contains("nothing was scored or posted"),
        "{out}"
    );
    assert_eq!(posted(&server, "run").await.len(), 1);

    // Live is sim86's: asked of Wokwi it is refused before the build.
    let o = m.ter_in(&server.uri(), &["run", "--sim", "--live"], &m.exercise());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && err.contains("bad_request"), "{err}");
    assert_eq!(posted(&server, "run").await.len(), 1);
}

/// The two pin changes of [`live_blinky`], and no more: a run whose
/// check window has not come round yet.
#[cfg(unix)]
fn live_before_the_checks() -> String {
    live_blinky()
        .lines()
        .take(2)
        .map(|l| format!("{l}\n"))
        .collect()
}

/// A run that ends before the check's time is up is not an attempt,
/// whatever ended it: no checks, only why it ended, and nothing posted.
/// One sim86 never started answers `/run` as before, except that no free
/// simulator is an error to try again on.
#[cfg(unix)]
#[tokio::test]
async fn a_live_run_that_ends_before_its_checks_is_not_posted_and_one_that_never_starts_is_a_run_record()
 {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_before_the_checks());
    let served = m.serve(&server.uri());

    // The page stops it.
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let mut events = LiveStream::open(&served, &id).await;
    let (_, before) = events.until("pin").await;
    assert_eq!(before, ["controls"]);
    assert_eq!(served.live_post(&id, "stop", json!({})).await.0, 200);
    let (ended, before) = events.until("ended").await;
    assert_eq!(ended, json!({"reason": "user-stop", "detail": ""}));
    assert!(!before.contains(&"checks".to_string()), "{before:?}");

    // Its limit is shorter than the check.
    let (_, _, v) = served.run(live_ask(2)).await;
    let mut events = LiveStream::open(&served, v["live"].as_str().unwrap()).await;
    let (ended, before) = events.until("ended").await;
    assert_eq!(ended["reason"], "limit");
    assert!(!before.contains(&"checks".to_string()), "{before:?}");

    // The board stops by itself, and sim86 fails under it.
    for (said, reason, detail) in [
        ("firmware-exit", "firmware-exit", "firmware-exit"),
        (
            "sim-exception: a non-electrical pin value on 'D13'",
            "sim-error",
            "a non-electrical pin value on 'D13'",
        ),
        ("duration", "limit", "duration"),
    ] {
        std::fs::write(
            m.sim86_bin().join("live-ends"),
            format!("{}\n", json!({"ev": "ended", "reason": said})),
        )
        .unwrap();
        let (status, _, v) = served.run(live_ask(60)).await;
        assert_eq!(status, 200, "{v}");
        let mut events = LiveStream::open(&served, v["live"].as_str().unwrap()).await;
        let (ended, before) = events.until("ended").await;
        assert_eq!(ended, json!({"reason": reason, "detail": detail}));
        assert!(!before.contains(&"checks".to_string()), "{before:?}");
    }
    assert!(
        posted(&server, "run").await.is_empty(),
        "none of them is an attempt"
    );
    assert!(
        ter_sdk::run::LastRun::load(&m.exercise())
            .unwrap()
            .is_none(),
        "nor the exercise's last run"
    );

    // sim86 has no simulator free: an error, nothing posted, no live run.
    let fails = m.sim86_bin().join("live-fails");
    std::fs::write(&fails, sim86_error("busy", "run refused: no-capacity")).unwrap();
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (503, Some("no_capacity")),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no free capacity"),
        "{v}"
    );
    assert!(posted(&server, "run").await.is_empty());
    // Any other refusal at the start is a run posted as not run.
    std::fs::write(&fails, sim86_error("board", "unknown part type 'Ledd'")).unwrap();
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        (&v["check_status"], v.get("live")),
        (&json!("not_run"), None),
        "{v}"
    );
    assert_eq!(posted(&server, "run").await.len(), 1);
    drop(served);

    // A build that fails: the run record, and no live run.
    let m = live_machine(BROKEN, &live_blinky());
    let served = m.serve(&server.uri());
    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["build_status"], "failed", "{v}");
    assert!(v.get("live").is_none(), "{v}");
    assert!(!m.sim86_bin().join("got-live-request.json").exists());
    drop(served);

    // Live is sim86's for now: a Wokwi run asked for live is refused
    // before anything is built, and a hardware one too.
    let m = Machine::with_exercise(GOOD, &["simulation", "hardware"]);
    std::fs::write(m.exercise().join("check.yaml"), SIM86_HEARTBEAT_CHECK).unwrap();
    let served = m.serve(&server.uri());
    for ask in [
        json!({"exercise_id": EXERCISE, "live": {"limit_s": 60}}),
        json!({"exercise_id": EXERCISE, "mode": "hardware", "live": {}}),
        json!({"exercise_id": EXERCISE, "venue": "sim86", "live": {"limit_s": 601}}),
    ] {
        let (status, _, v) = served.run(ask).await;
        assert_eq!(
            (status, v["error"]["code"].as_str()),
            (400, Some("bad_request")),
            "{v}"
        );
    }
    assert!(!m.exercise().join(".runs").exists(), "nothing was built");
}

/// A page that asks for sim86 where it cannot run is told why before
/// anything is built, and the run does not go to Wokwi instead.
#[cfg(unix)]
#[tokio::test]
async fn serve_refuses_sim86_where_it_cannot_run_and_does_not_fall_back() {
    let server = site("0.1.0", run_answer()).await;
    let ask = json!({"exercise_id": EXERCISE, "mode": "simulation", "venue": "sim86"});
    let not_built = |m: &Machine| !m.exercise().join(".runs").exists();

    // An exercise with a Wokwi circuit that would pass there, and no
    // sim86 board.
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let served = m.serve(&server.uri());
    let (status, _, v) = served.run(ask.clone()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "not_observable", "{v}");
    let said = v["error"]["message"].as_str().unwrap();
    assert!(said.contains("has no sim86 board"), "{said}");
    assert!(not_built(&m));
    // The same page naming no simulator runs it in Wokwi, as before.
    let (status, _, v) = served.run(json!({"exercise_id": EXERCISE})).await;
    assert_eq!((status, &v["venue"]), (200, &json!("wokwi")), "{v}");
    drop(served);

    // A sim86 board, and sim86 not installed on this machine.
    let m = sim86_blinky();
    std::fs::remove_dir_all(m.tools.path().join("sim86")).unwrap();
    let served = m.serve(&server.uri());
    let health = health_of(&served).await;
    assert_eq!(health["simulators"][1]["ready"], false, "{health}");
    let (status, _, v) = served.run(ask.clone()).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let said = v["error"]["message"].as_str().unwrap();
    assert!(said.contains("ter install sim86"), "{said}");
    assert_eq!(
        health["simulators"][1]["reason"], said,
        "/health gave the same reason before the page asked"
    );
    assert!(not_built(&m));
    drop(served);

    // A check sim86's board cannot show.
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        r#"{"user_button": {"part": "btn1"}}"#,
        "blinky.json",
    );
    let served = m.serve(&server.uri());
    let (status, _, v) = served.run(ask).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "not_observable", "{v}");
    assert!(not_built(&m));

    // A simulator is not a place to run hardware, and the other way round.
    let (status, _, v) = served
        .run(json!({"exercise_id": EXERCISE, "mode": "hardware", "venue": "sim86"}))
        .await;
    assert_eq!(
        (status, &v["error"]["code"]),
        (400, &json!("bad_request")),
        "{v}"
    );

    assert_eq!(posted(&server, "run").await.len(), 1, "only the Wokwi run");
}

/// `/health` lists each simulator a page can name and whether this
/// machine is set up for it: the tool and a key, nothing asked of the
/// simulator's service.
#[cfg(unix)]
#[tokio::test]
async fn health_lists_the_simulators_and_whether_each_is_ready() {
    let server = site("0.1.0", run_answer()).await;
    let names = |h: &Value| -> Vec<String> {
        h["simulators"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    };

    // Wokwi set up, sim86 not installed.
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).simulated(
        HEARTBEAT_CHECK,
        "heartbeat.vcd",
        42,
    );
    let served = m.serve(&server.uri());
    let health = health_of(&served).await;
    assert_eq!(names(&health), ["wokwi", "sim86"], "the default first");
    assert_eq!(
        health["simulators"][0],
        json!({"name": "wokwi", "ready": true})
    );
    assert_eq!(health["simulators"][1]["ready"], false);
    let why = health["simulators"][1]["reason"].as_str().unwrap();
    assert!(why.contains("ter install sim86"), "{why}");
    assert_eq!(
        health["venues"],
        json!(["simulation", "local", "bench"]),
        "as before"
    );
    drop(served);

    // sim86 installed with no key in the environment: the package is
    // asked whether it has one of its own.
    let mut m = sim86_blinky();
    m.sim86 = false;
    let served = m.serve(&server.uri());
    let health = health_of(&served).await;
    assert_eq!(health["simulators"][1]["ready"], false, "{health}");
    let why = health["simulators"][1]["reason"].as_str().unwrap();
    assert!(why.contains("No sim86 key on this machine"), "{why}");
    assert_eq!(
        m.sim86_saw("got-helper.py"),
        ter_telemetry::sim86::HELPER,
        "the helper this ter ships was asked"
    );
    drop(served);

    std::fs::write(m.sim86_bin().join("has-key"), "").unwrap();
    let served = m.serve(&server.uri());
    let health = health_of(&served).await;
    assert_eq!(
        health["simulators"][1],
        json!({"name": "sim86", "ready": true}),
        "{health}"
    );
    assert!(!health.to_string().contains(SIM86_KEY));
}

#[tokio::test]
async fn health_lists_the_venues_a_page_can_ask_for() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);
    let served = m.serve(&server.uri());
    let health: Value = reqwest::Client::new()
        .get(format!("{}/health", served.url))
        .header("origin", &served.page)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["venues"], json!(["simulation", "local", "bench"]));
}

/// A page run on a real shared bench: the owner's `ter serve --share`
/// flashes its board, the driver's `ter serve` posts. `bench` is what the
/// page names, if anything.
#[cfg(unix)]
async fn page_run_on_a_shared_bench(bench: Option<&str>) {
    page_run_connected_with(bench, Some(CODE)).await;
}

/// As [`page_run_on_a_shared_bench`], with the driver having run `ter
/// connect <code>` with `typed` first, or (`None`) connected only on the
/// Devices page. The site's `bench_using` gives the current code.
#[cfg(unix)]
async fn page_run_connected_with(bench: Option<&str>, typed: Option<&str>) -> MockServer {
    let bench_port = free_port();
    let server = bench_site(bench_port, 30).await;
    mount_using(&server, &[using_row(bench_port, true, true)]).await;
    let (owner, board) = owner_with_board(&["Hello world!\r\n"]);
    let _owner_serve = owner.serve_with(
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
    let driver = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    if let Some(typed) = typed {
        let (ok, v) = driver.json_in(&server.uri(), &["connect", typed], driver.courses.path());
        assert!(ok, "{v}");
    }
    let served = driver.serve(&server.uri());

    let mut body = json!({"exercise_id": EXERCISE, "mode": "hardware", "venue": "bench"});
    if let Some(b) = bench {
        body["bench"] = json!(b);
    }
    let (status, _, v) = served.run(body).await;
    board.thread.join().unwrap();
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["mode"], "hardware");
    assert_eq!(v["venue"], "bench", "{v}");
    assert_eq!(v["check_status"], "passed", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["mode"], "hardware");
    assert_eq!(run["venue"], "bench");
    assert!(
        owner.espflash_saw().contains("--chip esp32c3"),
        "the owner's board was flashed"
    );
    assert!(!owner.exercise().join(".runs").exists());
    let saved = std::fs::read_to_string(driver.config.path().join("benches.json")).unwrap();
    assert!(saved.contains(CODE), "the current code is kept: {saved}");
    server
}

/// `bench_using`'s answer: the one bench, connected and shared as asked.
fn using_row(bench_port: u16, connected: bool, sharing: bool) -> Value {
    json!({"name": BENCH, "label": "Desk", "board": "xiao-esp32c3", "owner_name": "Owner",
           "own": false, "status": "available", "sharing": sharing, "connected": connected,
           "url": format!("http://127.0.0.1:{bench_port}"),
           "share_code": if connected && sharing { json!(CODE) } else { json!(null) }})
}

async fn mount_using(server: &MockServer, rows: &[Value]) {
    Mock::given(method("GET"))
        .and(path(format!("{API}.bench_using")))
        .and(header("authorization", "Bearer good-token"))
        .respond_with(ok(json!({"ok": true, "benches": rows})))
        .with_priority(1)
        .mount(server)
        .await;
}

async fn asked(server: &MockServer, function: &str) -> usize {
    let wanted = format!("{API}.{function}");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == wanted)
        .count()
}

#[cfg(unix)]
#[tokio::test]
async fn a_bench_connected_only_on_the_devices_page_is_found_through_the_site() {
    for named in [Some(BENCH), None] {
        let server = page_run_connected_with(named, None).await;
        assert_eq!(asked(&server, "bench_using").await, 1, "{named:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn an_old_code_is_renewed_from_the_site_once_and_the_run_goes_on() {
    // Saved under the code the owner shared before; the bench refuses it.
    let server = page_run_connected_with(Some(BENCH), Some("OLDC-1111")).await;
    assert_eq!(asked(&server, "bench_using").await, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn the_code_in_the_reconnect_answer_is_used_without_asking_again() {
    let bench = fake_bench("xiao-esp32c3", &["serial", "reset"], None).await;
    let (server, driver, served) = {
        let port = bench.address().port();
        let server = bench_site(port, 30).await;
        let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
            BANNER_AND_BLINK,
            0,
            "/dev/ter-not-used",
        );
        let (fine, v) = driver.json_in(
            &server.uri(),
            &["connect", "OLDC-1111"],
            driver.courses.path(),
        );
        assert!(fine, "{v}");
        Mock::given(method("POST"))
            .and(path(format!("{API}.bench_reconnect")))
            .respond_with(ok(json!({"ok": true, "bench": BENCH, "label": "Desk",
                "board": "xiao-esp32c3", "owner_name": "Owner", "status": "available",
                "url": format!("http://127.0.0.1:{port}"), "share_code": CODE})))
            .with_priority(1)
            .mount(&server)
            .await;
        let served = driver.serve(&server.uri());
        (server, driver, served)
    };
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    // fake_bench answers only the current code; its /bench/run is not
    // there, so the run is posted not_run after the build.
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["venue"], "bench", "{v}");
    assert_eq!(asked(&server, "bench_using").await, 0);
    let saved = std::fs::read_to_string(driver.config.path().join("benches.json")).unwrap();
    assert!(
        saved.contains(CODE) && !saved.contains("OLDC-1111"),
        "{saved}"
    );
}

#[tokio::test]
async fn the_code_in_the_connect_answer_is_the_one_kept() {
    let server = bench_site(free_port(), 30).await;
    Mock::given(method("POST"))
        .and(path(format!("{API}.bench_connect")))
        .respond_with(ok(json!({"ok": true, "bench": BENCH, "label": "Desk",
            "board": "xiao-esp32c3", "owner_name": "Owner", "status": "available",
            "url": null, "share_code": CODE})))
        .with_priority(1)
        .mount(&server)
        .await;
    let m = Machine::with_exercise(GOOD, &["hardware"]);
    let (ok, v) = m.json_in(&server.uri(), &["connect", "gzts 2520"], m.courses.path());
    assert!(ok, "{v}");
    let saved = std::fs::read_to_string(m.config.path().join("benches.json")).unwrap();
    assert!(saved.contains(CODE), "{saved}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_bench_the_site_gives_no_code_for_says_why() {
    for (connected, sharing, says) in [
        (
            false,
            true,
            "You are not connected to Owner's bench Desk now",
        ),
        (true, false, "Owner stopped sharing Desk"),
    ] {
        let port = free_port();
        let server = bench_site(port, 30).await;
        mount_using(&server, &[using_row(port, connected, sharing)]).await;
        let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
            BANNER_AND_BLINK,
            0,
            "/dev/ter-not-used",
        );
        let served = driver.serve(&server.uri());
        let mut body: Value = serde_json::from_str(BENCH_RUN).unwrap();
        body["bench"] = json!(BENCH);
        let (status, _, v) = served.run(body).await;
        assert_eq!(status, 503, "{v}");
        assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains(says), "{msg}");
        assert!(posted(&server, "run").await.is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn serve_runs_on_the_newest_bench_when_the_page_names_none() {
    page_run_on_a_shared_bench(None).await;
}

#[cfg(unix)]
#[tokio::test]
async fn serve_runs_on_the_bench_the_page_names() {
    page_run_on_a_shared_bench(Some(BENCH)).await;
}

/// The owner's bench socket, played by a mock: `GET /bench` says `board`,
/// what it sees and who drives it.
async fn fake_bench(board: &str, provides: &[&str], driver: Option<&str>) -> MockServer {
    let bench = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bench"))
        .and(header("x-ter-share-code", CODE))
        .respond_with(ResponseTemplate::new(200).set_body_json(bench_info(board, provides, driver)))
        .mount(&bench)
        .await;
    bench
}

fn bench_info(board: &str, provides: &[&str], driver: Option<&str>) -> Value {
    json!({"service": "ter-bench", "version": "0.1.2", "board": board, "label": "Desk",
           "provides": provides, "driver": driver})
}

/// A driver connected to the bench at `bench`, with its `ter serve`.
#[cfg(unix)]
async fn driver_of(bench: &MockServer, check: &str) -> (MockServer, Machine, Served) {
    let port = bench.address().port();
    let server = bench_site(port, 30).await;
    let driver = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_board(
        check,
        0,
        "/dev/ter-not-used",
    );
    let (ok, v) = driver.json_in(&server.uri(), &["connect", CODE], driver.courses.path());
    assert!(ok, "{v}");
    let served = driver.serve(&server.uri());
    (server, driver, served)
}

const BENCH_RUN: &str = r#"{"exercise_id": "gpio-blinky--xiao-esp32c3-nostd",
    "mode": "hardware", "venue": "bench"}"#;

#[cfg(unix)]
#[tokio::test]
async fn serve_refuses_a_bench_this_machine_is_not_connected_to() {
    let bench = fake_bench("xiao-esp32c3", &["serial", "reset"], None).await;
    let (server, driver, served) = driver_of(&bench, BANNER_AND_BLINK).await;
    let mut body: Value = serde_json::from_str(BENCH_RUN).unwrap();
    body["bench"] = json!("b-9999");
    let (status, _, v) = served.run(body).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["error"]["code"], "unknown_bench", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("b-9999") && msg.contains("ter connect"),
        "{msg}"
    );
    assert!(posted(&server, "run").await.is_empty());
    assert!(!driver.exercise().join(".runs").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn serve_waits_for_a_busy_bench_then_answers_bench_busy() {
    let bench = fake_bench(
        "xiao-esp32c3",
        &["serial", "reset"],
        Some("ana@example.com"),
    )
    .await;
    let (server, driver, served) = driver_of(&bench, BANNER_AND_BLINK).await;
    let t = std::time::Instant::now();
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    assert_eq!(status, 409, "{v}");
    assert_eq!(v["error"]["code"], "bench_busy", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("ana@example.com is using Desk"), "{msg}");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(2),
        "it waited"
    );
    let asked = bench
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/bench")
        .count();
    assert!(asked >= 2, "asked again while waiting: {asked}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!driver.exercise().join(".runs").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_bench_freed_while_the_page_waits_takes_the_run() {
    let bench = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/bench"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bench_info(
            "xiao-esp32c3",
            &["serial", "reset"],
            Some("ana@example.com"),
        )))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&bench)
        .await;
    Mock::given(method("GET"))
        .and(path("/bench"))
        .respond_with(ResponseTemplate::new(200).set_body_json(bench_info(
            "xiao-esp32c3",
            &["serial", "reset"],
            None,
        )))
        .mount(&bench)
        .await;
    // The flash then fails on the owner's side: still the bench's run.
    Mock::given(method("POST"))
        .and(path("/bench/run"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({"error": {
            "code": "flash_failed", "message": "espflash could not connect", "output": ""}})))
        .mount(&bench)
        .await;
    let (server, _driver, served) = driver_of(&bench, BANNER_AND_BLINK).await;
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    assert_eq!(status, 200, "a posted run: {v}");
    assert_eq!(v["venue"], "bench", "{v}");
    assert_eq!(v["check_status"], "not_run", "{v}");
    let run = &posted(&server, "run").await[0]["payload"];
    assert_eq!(run["venue"], "bench");
    assert_eq!(run["mode"], "hardware");
}

#[cfg(unix)]
#[tokio::test]
async fn serve_answers_bench_offline_when_the_bench_does_not_answer() {
    let server = bench_site(free_port(), 30).await;
    let driver = Machine::with_exercise(GOOD, &["hardware"]).on_board(
        BANNER_AND_BLINK,
        0,
        "/dev/ter-not-used",
    );
    let (ok, v) = driver.json_in(&server.uri(), &["connect", CODE], driver.courses.path());
    assert!(ok, "{v}");
    let served = driver.serve(&server.uri());
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(v["error"]["code"], "bench_offline", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Owner's bench Desk is offline"),
        "{v}"
    );
    assert!(posted(&server, "run").await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn serve_refuses_a_bench_with_another_board() {
    let bench = fake_bench("xiao-esp32c6", &["serial", "reset"], None).await;
    let (server, _driver, served) = driver_of(&bench, BANNER_AND_BLINK).await;
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "wrong_board", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("xiao-esp32c6"),
        "{v}"
    );
    assert!(posted(&server, "run").await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_bench_that_can_see_none_of_the_checks_is_not_used() {
    let pins_only = "\
timeout_ms: 5500
assert:
  - id: heartbeat-rate
    pin: user_led
    toggles_per_s: { min: 3.6, max: 4.4 }
    window_ms: [900, 4900]
";
    let bench = fake_bench("xiao-esp32c3", &["serial", "reset"], None).await;
    let (server, driver, served) = driver_of(&bench, pins_only).await;
    let (status, _, v) = served.run(serde_json::from_str(BENCH_RUN).unwrap()).await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"]["code"], "not_observable", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("heartbeat-rate is graded only in simulation"),
        "{msg}"
    );
    assert!(posted(&server, "run").await.is_empty());
    assert!(!driver.exercise().join(".runs").exists());
}

/// What ter's helper printed for a real sim86 run, recorded 2026-11-23.
fn sim86_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sim86")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn a_sim86_run_is_recorded_judged_and_posted() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );

    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);

    assert!(ok, "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["mode"], "simulation");
    assert_eq!(p["venue"], "sim86");
    assert_eq!(p["build_status"], "passed");
    assert_eq!(p["check_status"], "passed", "{v}");
    assert_eq!(
        (&p["checks_seen"], &p["checks_total"]),
        (&json!(3), &json!(3))
    );
    assert_eq!(
        v["verdicts"][1]["observed"],
        "4 toggles per second (16 level changes)"
    );
    // sim86 times an edge to the nanosecond; ter keeps the microsecond.
    assert_eq!(
        v["verdicts"][2]["observed"],
        "8 pulses, 100.001 to 100.002 ms"
    );

    // The helper was asked for the LED's net on the exercise's own board,
    // with the program as built, for the check's time and a little more.
    let request: Value = serde_json::from_str(&m.sim86_saw("got-request.json")).unwrap();
    assert_eq!(request["watch"], json!(["mcu.D1"]));
    assert_eq!(
        request["board"],
        json!(m.exercise().join("sim86/board.json"))
    );
    let program = request["firmware"]["mcu"].as_str().unwrap();
    assert!(Path::new(program).is_file(), "{program}");
    assert_eq!(request["timeout_ms"], 5500);
    assert_eq!(request["duration_ms"], 5800);
    assert_eq!(request["scenario"], Value::Null);
    assert_eq!(m.sim86_saw("got-telemetry").trim(), "0");
    // The helper beside the package is the one this ter ships.
    assert_eq!(
        m.sim86_saw("got-helper.py"),
        ter_telemetry::sim86::HELPER,
        "written before the run"
    );

    let run = m.exercise().join(".runs/1");
    for f in [
        "sim86-request.json",
        "sim86.json",
        "sim86.err",
        "events.jsonl",
        "check.toml",
        "check.yaml",
        "build.log",
    ] {
        assert!(run.join(f).is_file(), "{f} is in the run folder");
    }
    let events = std::fs::read_to_string(run.join("events.jsonl")).unwrap();
    assert!(events.contains(r#""venue":"sim86""#), "{events}");
    // The first edge of the run is in the recording: 143.325 ms.
    assert!(
        events.contains(r#"{"t_us":143325,"kind":"pin_edge","name":"user_led","level":"high"}"#),
        "{events}"
    );
    // The key is the learner's and stays in the environment.
    for f in ["sim86-request.json", "sim86.json", "events.jsonl"] {
        let text = std::fs::read_to_string(run.join(f)).unwrap();
        assert!(!text.contains(SIM86_KEY), "{f}");
    }
    assert!(!v.to_string().contains(SIM86_KEY));

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
async fn a_sim86_run_of_a_folder_named_from_elsewhere_finds_its_files() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );
    let course = m.exercise().parent().unwrap().to_path_buf();

    // The helper runs in the run folder: every path it is given is whole.
    let (ok, v) = m.json_in(
        &server.uri(),
        &["run", "--sim", "sim86", &format!("../{COURSE}/{EXERCISE}")],
        &course,
    );

    assert!(ok, "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["check_status"], "passed", "{v}");
    let request: Value = serde_json::from_str(&m.sim86_saw("got-request.json")).unwrap();
    for path in [&request["board"], &request["firmware"]["mcu"]] {
        let path = Path::new(path.as_str().unwrap());
        assert!(path.is_absolute() && path.is_file(), "{path:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sim86_is_the_simulator_of_an_exercise_with_no_wokwi_circuit() {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );
    std::fs::remove_file(m.exercise().join("diagram.json")).unwrap();

    let o = m.ter_in(&server.uri(), &["run", "--sim"], &m.exercise());

    let (out, err) = (
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
    assert!(o.status.success(), "{out}{err}");
    assert!(err.contains("(simulation, then sim86)"), "{err}");
    assert!(err.contains("Running 5500 ms on sim86"), "{err}");
    assert!(out.contains("on sim86 in"), "{out}");
    assert_eq!(posted(&server, "run").await[0]["payload"]["venue"], "sim86");
}

#[cfg(unix)]
#[tokio::test]
async fn a_wrong_program_fails_the_right_check_on_sim86() {
    let server = site("0.1.0", run_answer()).await;
    // The same run with the LED never driven: no edge on its net.
    let mut answer: Value = serde_json::from_str(&sim86_fixture("blinky.json")).unwrap();
    answer["pins"]["mcu.D1"]["edges"] = json!([]);
    let m = Machine::with_exercise(GOOD, &["simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        &answer.to_string(),
    );

    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "check_failed", "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["check_status"], "failed");
    let ff: Value = serde_json::from_str(p["first_failure"].as_str().unwrap()).unwrap();
    assert_eq!(ff["id"], "heartbeat-rate");
    assert_eq!(ff["observed"], "0 toggles per second (0 level changes)");
    assert_eq!(v["verdicts"][0]["status"], "pass", "the boot line is there");
}

/// A sim86 run whose helper answers `answer`: posted as not run.
#[cfg(unix)]
async fn sim86_not_run(answer: &str) -> (Value, Value) {
    let server = site("0.1.0", run_answer()).await;
    let m = Machine::with_exercise(GOOD, &["simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        answer,
    );

    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);

    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let p = posted(&server, "run").await[0]["payload"].clone();
    assert_eq!(p["venue"], "sim86");
    assert_eq!(p["check_status"], "not_run", "never failed");
    assert!(p.get("first_failure").is_none());
    let tail = p["transcript_tail"].as_str().unwrap();
    assert_eq!(tail.lines().next(), Some("venue_unavailable"));
    assert!(!v.to_string().contains(SIM86_KEY) && !tail.contains(SIM86_KEY));
    assert!(!m.exercise().join(".runs/1/check.toml").exists());
    (v, p)
}

#[cfg(unix)]
fn sim86_error(kind: &str, message: &str) -> String {
    json!({"helper": 1, "ok": false, "sim86": "0.1.4",
           "error": {"kind": kind, "message": message}})
    .to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn every_sim86_failure_is_posted_as_not_run_with_sim86s_words() {
    for (kind, said, ours) in [
        (
            "busy",
            "No simulator came free in 5 minutes.",
            "Every sim86 simulator is busy",
        ),
        (
            "credits",
            "run refused: out of credits",
            "no credits left for a run",
        ),
        (
            "auth",
            "API key rejected (invalid or missing API key)",
            "sim86 refused your key",
        ),
        (
            "image",
            "fw.bin is not a format this chip runs (esp-merged-bin)",
            "does not take the program as built",
        ),
        (
            "board",
            "unknown part type 'Ledd'",
            "sim86/board.json needs fixing in the curriculum",
        ),
        ("version", "API error 426", "ter install sim86"),
        (
            "ended",
            "run r ended (duration) before it answered",
            "ended before the check's time was up",
        ),
        (
            "incomplete",
            "mcu.D1: the recording between 0 and 1000 ns is a summary, not every edge",
            "could not give every edge of a watched pin, so the run is not judged",
        ),
    ] {
        let (v, p) = sim86_not_run(&sim86_error(kind, said)).await;
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains(ours), "{kind}: {msg}");
        assert!(msg.contains("posted as not run"), "{msg}");
        let tail = p["transcript_tail"].as_str().unwrap();
        assert!(tail.contains(said), "sim86's own words kept: {tail}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_sim86_run_that_did_not_start_paused_at_zero_is_not_judged() {
    // The whole recording of a passing run, said to start 742 ms in.
    let mut answer: Value = serde_json::from_str(&sim86_fixture("blinky.json")).unwrap();
    answer["started_ns"] = json!(742_000_000u64);
    let (v, _) = sim86_not_run(&answer.to_string()).await;
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("did not hold the board at its start") && msg.contains("742.000 ms"),
        "{msg}"
    );
    // And a helper that printed nothing it should have.
    let (v, _) = sim86_not_run("Traceback (most recent call last):").await;
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("helper failed"), "{msg}");
}

/// `ter run --sim sim86` stopped before the build: nothing built, nothing
/// posted.
#[cfg(unix)]
async fn sim86_refused(m: &Machine, code: &str) -> String {
    let server = site("0.1.0", run_answer()).await;
    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], code, "{v}");
    assert!(posted(&server, "run").await.is_empty());
    assert!(!m.exercise().join(".runs").exists());
    assert!(!v.to_string().contains(SIM86_KEY));
    v["error"]["message"].as_str().unwrap().to_string()
}

#[cfg(unix)]
fn sim86_blinky() -> Machine {
    Machine::with_exercise(GOOD, &["hardware", "simulation"]).on_sim86(
        SIM86_HEARTBEAT_CHECK,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    )
}

#[cfg(unix)]
#[tokio::test]
async fn sim86_not_installed_no_key_or_no_credits_stops_before_the_build() {
    let m = sim86_blinky();
    std::fs::remove_dir_all(m.tools.path().join("sim86")).unwrap();
    let msg = sim86_refused(&m, "venue_unavailable").await;
    assert!(msg.contains("ter install sim86"), "{msg}");
    assert!(msg.contains("needs Python 3.9 or newer"), "{msg}");

    let balance = |m: &Machine, text: &str| {
        std::fs::write(m.sim86_bin().join("balance.json"), text).unwrap();
    };
    let m = sim86_blinky();
    balance(
        &m,
        &sim86_error(
            "no_key",
            "no API key: set SIM86_API_KEY, or run `sim86 login`",
        ),
    );
    let msg = sim86_refused(&m, "venue_unavailable").await;
    assert!(msg.contains("No sim86 key on this machine"), "{msg}");

    balance(&m, &sim86_error("auth", "API key rejected"));
    let msg = sim86_refused(&m, "venue_unavailable").await;
    assert!(msg.contains("sim86 refused your key"), "{msg}");

    balance(
        &m,
        &SIM86_BALANCE.replace("\"credits\": 20184", "\"credits\": 0"),
    );
    let msg = sim86_refused(&m, "venue_unavailable").await;
    assert!(msg.contains("no credits left"), "{msg}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_balance_sim86_does_not_give_does_not_stop_the_run() {
    let server = site("0.1.0", run_answer()).await;
    let m = sim86_blinky();
    std::fs::write(
        m.sim86_bin().join("balance.json"),
        sim86_error("api", "API error 500"),
    )
    .unwrap();
    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);
    assert!(ok, "{v}");
    assert_eq!(
        posted(&server, "run").await[0]["payload"]["check_status"],
        "passed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_folder_without_a_sim86_board_or_a_pin_without_a_wire_is_not_run_on_sim86() {
    // No sim86 folder at all.
    let m = sim86_blinky();
    std::fs::remove_dir_all(m.exercise().join("sim86")).unwrap();
    let msg = sim86_refused(&m, "not_observable").await;
    assert!(msg.contains("has no sim86 board"), "{msg}");
    assert!(msg.contains("Run it in Wokwi: `ter run --sim`"), "{msg}");

    // The board without its pin map.
    let m = sim86_blinky();
    std::fs::remove_file(m.exercise().join("sim86/pins.json")).unwrap();
    let msg = sim86_refused(&m, "not_observable").await;
    assert!(msg.contains("has no sim86 board"), "{msg}");

    // A check on the buzzer, which no wire in the board takes anywhere.
    let check = SIM86_HEARTBEAT_CHECK.replace("pin: user_led\n    pulse", "pin: buzzer\n    pulse");
    assert_ne!(check, SIM86_HEARTBEAT_CHECK);
    let m = Machine::with_exercise(GOOD, &["simulation"]).on_sim86(
        &check,
        SIM86_UFERRIS,
        SIM86_UFERRIS_PINS,
        "blinky.json",
    );
    let msg = sim86_refused(&m, "not_observable").await;
    assert!(
        msg.contains("buzzer is mcu.D2, which has no wire to another part"),
        "{msg}"
    );
    assert!(!m.sim86_bin().join("got-request.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_press_on_sim86_is_planned_before_the_board_starts_and_checked_after() {
    let server = site("0.1.0", run_answer()).await;
    let check = "timeout_ms: 3000\nsetup:\n  - press: user_button\n    at_ms: 1000\n    hold_ms: 200\nassert:\n  - id: led-on\n    pin: user_led\n    is: high\n    after_ms: 1100\n";
    let m = Machine::with_exercise(GOOD, &["simulation"]).on_sim86(
        check,
        SIM86_NUCLEO,
        SIM86_NUCLEO_PINS,
        "button-toggle.json",
    );

    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);

    assert!(ok, "{v}");
    assert_eq!(v["verdicts"][0]["status"], "pass", "{v}");
    let request: Value = serde_json::from_str(&m.sim86_saw("got-request.json")).unwrap();
    assert_eq!(
        request["scenario"]["events"],
        json!([
            {"at": "1000ms", "env": {"part": "mcu", "input": "button", "value": 1.0}},
            {"at": "1200ms", "env": {"part": "mcu", "input": "button", "value": 0.0}},
        ])
    );
    assert_eq!(request["watch"], json!(["mcu.D13"]));

    // The same recording without the press among what sim86 applied: the
    // LED still came on, and the run is not judged.
    let mut answer: Value = serde_json::from_str(&sim86_fixture("button-toggle.json")).unwrap();
    answer["applied"] = json!([{"atNs": 0, "event": "reached"}]);
    std::fs::write(m.sim86_bin().join("run.json"), answer.to_string()).unwrap();
    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "venue_unavailable", "{v}");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("did not apply the setup's input"), "{msg}");
    let runs = posted(&server, "run").await;
    assert_eq!(runs[1]["payload"]["check_status"], "not_run");
}

/// A heartbeat check longer than the 10 s sim86 holds of a net.
const SIM86_LONG_CHECK: &str = "\
timeout_ms: 13500
assert:
  - id: boot
    serial_contains: \"ESP-ROM:esp32c3\"
    within_ms: 2000
  - id: heartbeat-rate
    pin: user_led
    toggles_per_s: { min: 3.6, max: 4.4 }
    window_ms: [900, 12900]
  - id: flash-width
    pin: user_led
    pulse_width_ms: { min: 80, max: 120 }
    window_ms: [900, 12900]
";

/// The heartbeat exercise with [`SIM86_LONG_CHECK`], run by ter's own
/// helper on the stand-in sim86 package.
#[cfg(unix)]
fn long_sim86_machine() -> Machine {
    let m = live_machine(GOOD, "").with_sim86_package();
    std::fs::write(m.exercise().join("check.yaml"), SIM86_LONG_CHECK).unwrap();
    m
}

/// `ter serve` with `envs` set.
fn serve_with_env(m: &Machine, site: &str, envs: &[(&str, &str)]) -> Served {
    let mut cmd = m.command(site, &["serve", "--port", "0"], m.courses.path());
    cmd.envs(envs.iter().copied());
    m.serve_from(cmd, site, &[])
}

/// Wait up to `secs` for `file` beside the stand-in for sim86 to hold
/// `text`.
async fn sim86_says(m: &Machine, file: &str, text: &str, secs: u64) -> bool {
    let path = m.sim86_bin().join(file);
    for _ in 0..secs * 10 {
        if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(text)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// sim86 answers for 10 s windows and holds a net's last 10 s only: the
/// helper runs the board a piece at a time and reads each piece while it
/// is there.
#[cfg(unix)]
#[tokio::test]
async fn a_sim86_run_longer_than_sim86_holds_is_read_in_pieces_and_judged() {
    let server = site("0.1.0", run_answer()).await;
    let m = long_sim86_machine();

    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);

    assert!(ok, "{v}");
    let p = &posted(&server, "run").await[0]["payload"];
    assert_eq!(p["check_status"], "passed", "{v}");
    let events = std::fs::read_to_string(m.exercise().join(".runs/1/events.jsonl")).unwrap();
    for t_us in [143_000, 13_443_000] {
        assert!(
            events.contains(&format!(r#"{{"t_us":{t_us},"kind":"pin_edge""#)),
            "{t_us}: {events}"
        );
    }
    assert!(m.sim86_saw("stopped").contains("fake-run-1"));
}

/// A live run longer than sim86 holds is read as it goes, judged, and
/// stopped on sim86 by the helper when the page says so.
#[cfg(unix)]
#[tokio::test]
async fn a_long_live_run_is_read_as_it_goes_and_stopped_on_sim86() {
    let server = site("0.1.0", run_answer()).await;
    let m = long_sim86_machine();
    std::fs::write(m.sim86_bin().join("speed"), "4").unwrap();
    let served = m.serve(&server.uri());

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["sim86_run"], "fake-run-1", "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let mut events = LiveStream::open(&served, &id).await;
    let (checks, before) = events.until("checks").await;
    assert_eq!(checks["check_status"], "passed", "{checks}");
    assert_eq!(checks["events_end_us"], 13_500_000);
    assert!(
        before.iter().filter(|e| *e == "pin").count() > 50,
        "{before:?}"
    );
    assert!(before.contains(&"serial".to_string()), "{before:?}");

    assert_eq!(
        served.live_post(&id, "stop", json!({})).await,
        (200, json!({"ok": true}))
    );
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended, json!({"reason": "user-stop", "detail": ""}));
    assert!(m.sim86_saw("stopped").contains("fake-run-1"));
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The page pauses the board and resumes it: a paused board holds its
/// time, the pause does not count against the run's limit, which moves on
/// by as much, and a run paused for longer than it may be ends at its
/// limit.
#[cfg(unix)]
#[tokio::test]
async fn a_paused_live_run_holds_its_time_and_its_limit_moves_on_when_it_resumes() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, "").with_sim86_package();
    let served = serve_with_env(&m, &server.uri(), &[("TER_LIVE_PAUSE_SECS", "6")]);

    let (status, _, v) = served.run(live_ask(3)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let ends_at = v["ends_at"].as_u64().unwrap();
    let mut events = LiveStream::open(&served, &id).await;
    events.until("pin").await;

    let (status, paused) = served.live_post(&id, "pause", json!({})).await;
    assert_eq!(status, 200, "{paused}");
    assert_eq!(paused["ok"], true, "{paused}");
    let t_ms = paused["t_ms"].as_f64().unwrap();
    assert!(t_ms > 0.0, "{paused}");
    let pause_ends_at = paused["pause_ends_at"].as_u64().unwrap();
    assert!(
        pause_ends_at.abs_diff(unix_ms_now() + 6_000) < 1_000,
        "{paused}"
    );
    let (event, _) = events.until("paused").await;
    assert_eq!(event, json!({"t_ms": t_ms, "pause_ends_at": pause_ends_at}));

    // Paused past the run's limit: it goes on, and holds its time.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(unix_ms_now() > ends_at);
    let (status, again) = served.live_post(&id, "pause", json!({})).await;
    assert_eq!((status, &again["t_ms"]), (200, &json!(t_ms)), "{again}");
    // A page that starts to listen meanwhile is told the board is paused.
    let mut late = LiveStream::open(&served, &id).await;
    let (event, _) = late.until("paused").await;
    assert_eq!(event["t_ms"], json!(t_ms));
    drop(late);

    let (status, resumed) = served.live_post(&id, "resume", json!({})).await;
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["t_ms"], json!(t_ms), "it runs on from where it was");
    let moved = resumed["ends_at"].as_u64().unwrap();
    assert!(moved >= ends_at + 4_000, "{resumed} {ends_at}");
    let (event, before) = events.until("resumed").await;
    assert_eq!(event, json!({"t_ms": t_ms, "ends_at": moved}));
    assert!(!before.contains(&"ended".to_string()), "{before:?}");
    assert_eq!(events.until("limit").await.0, json!({"ends_at": moved}));
    // Resuming a board that runs changes nothing.
    let (status, v) = served.live_post(&id, "resume", json!({})).await;
    assert_eq!((status, &v["ends_at"]), (200, &json!(moved)), "{v}");

    // Paused again, the run has a second or two of its pauses left: then
    // it ends at its limit, paused.
    let (status, paused) = served.live_post(&id, "pause", json!({})).await;
    assert_eq!(status, 200, "{paused}");
    assert!(
        paused["pause_ends_at"].as_u64().unwrap() < unix_ms_now() + 2_500,
        "{paused}"
    );
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended["reason"], "limit", "{ended}");
    assert!(unix_ms_now() < moved, "the pauses ran out, not the limit");
    assert!(sim86_says(&m, "stopped", "fake-run-1", 10).await);
    let (status, v) = served.live_post(&id, "resume", json!({})).await;
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (409, Some("ended")),
        "{v}"
    );
}

/// The page starts the board again from its time 0: a new sim86 run with
/// the program already built, under the same live run. A reset before the
/// check's window has run leaves one attempt to post, from the first run
/// folder; a reset after it is a second attempt, in a folder of its own.
#[cfg(unix)]
#[tokio::test]
async fn a_reset_live_run_starts_a_new_sim86_run_from_zero_and_is_judged_again() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, "").with_sim86_package();
    std::fs::write(m.sim86_bin().join("speed"), "2").unwrap();
    let served = m.serve(&server.uri());

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["sim86_run"], "fake-run-1", "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let ends_at = v["ends_at"].as_u64().unwrap();
    let mut events = LiveStream::open(&served, &id).await;
    events.until("pin").await;

    // Before the window has run: nothing is posted for the first board.
    let (status, reset) = served.live_post(&id, "reset", json!({})).await;
    assert_eq!(status, 200, "{reset}");
    assert_eq!(
        reset,
        json!({"ok": true, "run": "fake-run-2", "ends_at": ends_at})
    );
    let (event, before) = events.until("reset").await;
    assert_eq!(event, json!({"run": "fake-run-2", "ends_at": ends_at}));
    assert!(!before.contains(&"checks".to_string()), "{before:?}");
    assert!(sim86_says(&m, "stopped", "fake-run-1", 10).await);
    let (pin, _) = events.until("pin").await;
    assert_eq!(pin["t_ms"], json!(0.0), "the new board starts at 0: {pin}");
    let (checks, _) = events.until("checks").await;
    assert_eq!(checks["check_status"], "passed", "{checks}");
    assert_eq!(posted(&server, "run").await.len(), 1);
    assert!(!m.exercise().join(".runs/2").exists());

    // After it, paused: the new board runs, and the pause moved the end on.
    let (status, _) = served.live_post(&id, "pause", json!({})).await;
    assert_eq!(status, 200);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (status, reset) = served.live_post(&id, "reset", json!({})).await;
    assert_eq!(status, 200, "{reset}");
    assert_eq!(reset["run"], "fake-run-3", "{reset}");
    assert!(
        reset["ends_at"].as_u64().unwrap() >= ends_at + 1_000,
        "{reset}"
    );
    events.until("reset").await;
    let (checks, before) = events.until("checks").await;
    assert_eq!(checks["check_status"], "passed", "{checks}");
    assert!(!before.contains(&"paused".to_string()), "{before:?}");
    assert_eq!(posted(&server, "run").await.len(), 2);
    assert!(m.exercise().join(".runs/2/build.log").is_file());
    assert!(m.exercise().join(".runs/2/events.jsonl").is_file());

    assert_eq!(
        served.live_post(&id, "stop", json!({})).await,
        (200, json!({"ok": true}))
    );
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended["reason"], "user-stop", "{ended}");
    assert!(sim86_says(&m, "stopped", "fake-run-3", 10).await);
    let runs = m.sim86_saw("runs");
    assert_eq!(runs.lines().count(), 3, "{runs}");
    assert!(!m.exercise().join(".runs/1/firmware.bin").exists());
}

/// A board with an LDR, a potentiometer and a button: the LDR starts at
/// 250 lux by its `config.inputs`, which sim86 is not sent.
const SIM86_CONTROLS_BOARD: &str = r#"{"name": "ter-uferris-c3-basic",
 "parts": [{"id": "mcu", "type": "XiaoESP32C3", "firmware": "fw.bin"},
           {"id": "led1", "type": "Led", "config": {"color": "red"}},
           {"id": "btn1", "type": "Btn", "label": "S1"},
           {"id": "ldr1", "type": "Ldr", "config": {"inputs": {"lux": 250}, "pull": 1}},
           {"id": "pot1", "type": "Potentiometer"}],
 "wires": [["mcu.D1", "led1.+"], ["led1.-", "mcu.GND"], ["btn1.L1", "mcu.D3"], ["btn1.L2", "mcu.GND"]]}
"#;

/// sim86's catalog for those parts, as it gives them.
const SIM86_CONTROLS_CATALOG: &str = r#"{
 "XiaoESP32C3": {"name": "Seeed XIAO ESP32-C3", "inputs": [{"default": 0.0, "label": "BOOT button (GPIO9)", "max": 1.0, "min": 0.0, "name": "boot", "step": 1.0, "unit": "", "widget": "button"}]},
 "Btn": {"name": "Button", "description": "4-leg push button. Events: 'pressed', 'released'."},
 "Ldr": {"name": "Photoresistor (LDR)", "description": "R = 15 k (lux/10)^-0.7.", "inputs": [{"default": 100.0, "label": "Light", "max": 10000.0, "min": 0.1, "name": "lux", "step": 0.1, "unit": "lux"}]},
 "Potentiometer": {"name": "Potentiometer", "inputs": [{"default": 0.5, "label": "Position", "max": 1.0, "min": 0.0, "name": "position", "step": 0.01, "unit": ""}]}
}"#;

fn controls_machine(board: &str) -> Machine {
    let m = live_machine(GOOD, "").with_sim86_package();
    std::fs::write(m.exercise().join("sim86/board.json"), board).unwrap();
    std::fs::write(m.sim86_bin().join("parts"), SIM86_CONTROLS_CATALOG).unwrap();
    m
}

/// What the stand-in sim86 was sent, a line each.
fn sim86_events(m: &Machine) -> Vec<Value> {
    std::fs::read_to_string(m.sim86_bin().join("events"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The page is sent the parts it can change, from sim86's catalog, sets
/// an input and presses a button, running or paused; ter refuses what no
/// part takes, and a reset starts the parts again as the board file says.
#[cfg(unix)]
#[tokio::test]
async fn the_page_sets_a_parts_input_and_sends_it_events_as_the_catalog_says() {
    let server = site("0.1.0", run_answer()).await;
    let m = controls_machine(SIM86_CONTROLS_BOARD);
    let served = m.serve(&server.uri());

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let mut events = LiveStream::open(&served, &id).await;
    let (name, controls) = events.next().await.unwrap();
    assert_eq!(name, "controls");
    let ldr = |lux: i64| {
        json!({"id": "ldr1", "label": "Photoresistor (LDR)", "type": "Ldr", "controls": [
            {"kind": "input", "name": "lux", "label": "Light", "unit": "lux",
             "min": 0.1, "max": 10000.0, "step": 0.1, "value": lux}]})
    };
    let parts = json!({"parts": [
        {"id": "mcu", "label": "Seeed XIAO ESP32-C3", "type": "XiaoESP32C3", "controls": [
            {"kind": "input", "name": "boot", "label": "BOOT button (GPIO9)", "unit": "",
             "min": 0.0, "max": 1.0, "step": 1.0, "widget": "button", "value": 0.0}]},
        {"id": "btn1", "label": "S1", "type": "Btn", "controls": [
            {"kind": "action", "name": "events", "actions": ["pressed", "released"]}]},
        ldr(250),
        {"id": "pot1", "label": "Potentiometer", "type": "Potentiometer", "controls": [
            {"kind": "input", "name": "position", "label": "Position", "unit": "",
             "min": 0.0, "max": 1.0, "step": 0.01, "value": 0.5}]},
    ]});
    assert_eq!(controls, parts);
    // The LDR's start is set at 0, before the board runs, and is not part
    // of the config sim86 is sent.
    assert_eq!(
        sim86_events(&m),
        [json!({"run": "fake-run-1", "part": "ldr1", "action": "input:lux=250", "t_ns": 0})]
    );
    let configs: Value =
        serde_json::from_str(m.sim86_saw("configs").lines().next().unwrap()).unwrap();
    assert_eq!(configs["ldr1"], json!({"pull": 1}));
    events.until("pin").await;

    let (status, set) = served
        .live_post(
            &id,
            "input",
            json!({"part": "ldr1", "name": "lux", "value": 500}),
        )
        .await;
    assert_eq!(status, 200, "{set}");
    let t_ms = set["t_ms"].as_f64().unwrap();
    assert!(t_ms > 0.0, "{set}");
    assert_eq!(
        set,
        json!({"ok": true, "part": "ldr1", "name": "lux", "value": 500, "t_ms": t_ms})
    );
    let (event, _) = events.until("input").await;
    assert_eq!(
        event,
        json!({"part": "ldr1", "name": "lux", "value": 500, "t_ms": t_ms})
    );

    // Paused, the button is pressed at the very time the board holds.
    let (_, paused) = served.live_post(&id, "pause", json!({})).await;
    let (status, pressed) = served
        .live_post(&id, "action", json!({"part": "btn1", "action": "pressed"}))
        .await;
    assert_eq!(status, 200, "{pressed}");
    assert_eq!(
        pressed,
        json!({"ok": true, "part": "btn1", "action": "pressed", "t_ms": paused["t_ms"]})
    );
    let (event, _) = events.until("action").await;
    assert_eq!(
        event,
        json!({"part": "btn1", "action": "pressed", "t_ms": paused["t_ms"]})
    );
    let sent = sim86_events(&m);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[1]["action"], "input:lux=500");
    assert_eq!(
        (&sent[2]["part"], &sent[2]["action"]),
        (&json!("btn1"), &json!("pressed"))
    );
    served.live_post(&id, "resume", json!({})).await;

    // What no part takes is refused, and nothing is sent.
    for (what, body, said) in [
        (
            "input",
            json!({"part": "ldr1", "name": "lux", "value": 20000}),
            "lux is from 0.1 to 10000 lux.",
        ),
        (
            "input",
            json!({"part": "ldr1", "name": "light", "value": 1}),
            "ldr1 has no input \"light\"; it has lux.",
        ),
        (
            "input",
            json!({"part": "led1", "name": "lux", "value": 1}),
            "No part \"led1\" of this board takes an input; these do: mcu, ldr1, pot1.",
        ),
        (
            "action",
            json!({"part": "btn1", "action": "click"}),
            "btn1 takes the events pressed, released; not \"click\".",
        ),
    ] {
        let (status, v) = served.live_post(&id, what, body).await;
        assert_eq!(
            (
                status,
                v["error"]["code"].as_str(),
                v["error"]["message"].as_str()
            ),
            (400, Some("bad_request"), Some(said)),
            "{v}"
        );
    }
    assert_eq!(sim86_events(&m).len(), 3);

    // A page that starts to listen now sees the LDR as it was set.
    let mut late = LiveStream::open(&served, &id).await;
    let (name, now) = late.next().await.unwrap();
    assert_eq!((name.as_str(), &now["parts"][2]), ("controls", &ldr(500)));
    drop(late);

    // A reset starts the LDR again at 250, on the new board at 0.
    let (status, reset) = served.live_post(&id, "reset", json!({})).await;
    assert_eq!(status, 200, "{reset}");
    events.until("reset").await;
    let (name, again) = events.next().await.unwrap();
    assert_eq!((name.as_str(), &again), ("controls", &parts));
    assert_eq!(
        sim86_events(&m)[3],
        json!({"run": "fake-run-2", "part": "ldr1", "action": "input:lux=250", "t_ns": 0})
    );

    served.live_post(&id, "stop", json!({})).await;
    events.until("ended").await;
    assert!(sim86_says(&m, "stopped", "fake-run-2", 10).await);
    // The catalog was asked once a part type: the helper keeps it.
    let looked = m.sim86_saw("looked-up");
    assert_eq!(
        looked.lines().filter(|l| *l == "Ldr").count(),
        1,
        "{looked}"
    );
}

/// A recorded run sets the parts' starting inputs too, and a board whose
/// starting input the catalog does not allow does not run.
#[cfg(unix)]
#[tokio::test]
async fn a_recorded_sim86_run_starts_its_parts_inputs_and_refuses_a_wrong_one() {
    let server = site("0.1.0", run_answer()).await;
    let m = controls_machine(SIM86_CONTROLS_BOARD);
    let (ok, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);
    assert!(ok, "{v}");
    assert_eq!(
        sim86_events(&m),
        [json!({"run": "fake-run-1", "part": "ldr1", "action": "input:lux=250", "t_ns": 0})]
    );

    let m = controls_machine(&SIM86_CONTROLS_BOARD.replace(r#"{"lux": 250}"#, r#"{"lux": -5}"#));
    let (_, v) = m.json(&server.uri(), &["run", "--sim", "sim86"]);
    assert!(
        v.to_string()
            .contains("ldr1 config.inputs: lux is from 0.1 to 10000.0"),
        "{v}"
    );
    assert!(!m.sim86_bin().join("runs").exists());
}

/// ter's own helper streams the board's other wired pins by the chip's
/// names from sim86's catalog, and a pin sim86 will not observe holds up
/// neither the run nor its checks.
#[cfg(unix)]
#[tokio::test]
async fn the_helper_shows_the_wired_pins_and_a_refused_one_changes_nothing() {
    let server = site("0.1.0", run_answer()).await;
    for refused in [false, true] {
        let m = long_sim86_machine();
        std::fs::write(m.exercise().join("check.yaml"), SIM86_HEARTBEAT_CHECK).unwrap();
        std::fs::write(
            m.sim86_bin().join("catalog"),
            r#"[{"name": "GND"}, {"name": "D1", "signal": "GPIO3"}, {"name": "D3", "signal": "GPIO5"}]"#,
        )
        .unwrap();
        if refused {
            std::fs::write(m.sim86_bin().join("refuse-observe"), "mcu.D3").unwrap();
        }
        let served = m.serve(&server.uri());
        let (status, _, v) = served.run(live_ask(60)).await;
        assert_eq!(status, 200, "{v}");
        let id = v["live"].as_str().unwrap().to_string();
        let mut events = LiveStream::open(&served, &id).await;
        let mut pins = Vec::new();
        let checks = loop {
            let (name, data) = events.next().await.unwrap();
            match name.as_str() {
                "pin" => pins.push(data),
                "checks" => break data,
                _ => {}
            }
        };
        assert_eq!(checks["check_status"], "passed", "{checks}");
        let shown: Vec<&Value> = pins.iter().filter(|p| p["net"] == "mcu.D3").collect();
        if refused {
            assert!(shown.is_empty(), "{shown:?}");
        } else {
            assert_eq!(
                shown,
                [&json!({"t_ms": 0.0, "pin": "GPIO5", "name": "D3", "net": "mcu.D3", "level": 0})]
            );
        }
        assert!(
            pins.iter()
                .filter(|p| p["net"] == "mcu.D1")
                .all(|p| p["name"] == "user_led" && p["pin"] == "GPIO3"),
            "{pins:?}"
        );
        let observed = m.sim86_saw("observed");
        assert!(observed.starts_with("[\"mcu.D1\"]\n"), "{observed}");
        assert_eq!(
            served.live_post(&id, "stop", json!({})).await,
            (200, json!({"ok": true}))
        );
        events.until("ended").await;
        assert!(m.sim86_saw("stopped").contains("fake-run-1"));
    }
}

/// A helper stuck waiting on sim86 still stops the sim86 run the moment
/// ter says stop, and ter kills it once it has waited long enough.
#[cfg(unix)]
#[tokio::test]
async fn a_helper_stuck_on_sim86_stops_the_run_at_once_and_is_killed() {
    let server = site("0.1.0", run_answer()).await;
    let m = long_sim86_machine();
    std::fs::write(m.sim86_bin().join("hang-observe"), "").unwrap();
    let served = serve_with_env(&m, &server.uri(), &[("TER_LIVE_STOP_PATIENCE_SECS", "2")]);

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let mut events = LiveStream::open(&served, &id).await;
    let asked = std::time::Instant::now();
    served.live_post(&id, "stop", json!({})).await;
    assert!(
        sim86_says(&m, "stopped", "fake-run-1", 2).await,
        "stopped on sim86 before ter gives up on the helper"
    );
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended, json!({"reason": "user-stop", "detail": ""}));
    assert!(
        asked.elapsed() < Duration::from_secs(15),
        "{:?}",
        asked.elapsed()
    );
}

/// ter gives up on a run sim86 is slow to make; the helper stops it as
/// soon as it exists.
#[cfg(unix)]
#[tokio::test]
async fn a_run_sim86_is_slow_to_make_is_stopped_once_it_exists() {
    let server = site("0.1.0", run_answer()).await;
    let m = long_sim86_machine();
    std::fs::write(m.sim86_bin().join("slow-start"), "3").unwrap();
    let served = serve_with_env(&m, &server.uri(), &[("TER_LIVE_START_SECS", "1")]);

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["check_status"], "not_run", "{v}");
    assert!(v.get("live").is_none(), "{v}");
    assert!(m.sim86_saw("runs").contains("fake-run-1"));
    assert!(sim86_says(&m, "stopped", "fake-run-1", 5).await);
}

/// A site that does not answer holds up neither the run's stop nor its
/// end: the checks say the site did not answer.
#[cfg(unix)]
#[tokio::test]
async fn a_site_that_does_not_answer_holds_up_no_live_run() {
    let server = site("0.1.0", run_answer().set_delay(Duration::from_secs(10))).await;
    let m = live_machine(GOOD, &live_blinky());
    let served = serve_with_env(&m, &server.uri(), &[("TER_LIVE_POST_SECS", "2")]);

    let (status, _, v) = served.run(live_ask(60)).await;
    assert_eq!(status, 200, "{v}");
    let id = v["live"].as_str().unwrap().to_string();
    let mut events = LiveStream::open(&served, &id).await;
    events.until("serial").await;
    // The recording follows the serial line; the post is then on its way.
    tokio::time::sleep(Duration::from_millis(300)).await;
    served.live_post(&id, "stop", json!({})).await;
    assert!(
        sim86_says(&m, "got-stop", "stop", 1).await,
        "the helper is told at once"
    );
    let (checks, _) = events.until("checks").await;
    assert_eq!(checks["error"]["code"], "network_error", "{checks}");
    let (ended, _) = events.until("ended").await;
    assert_eq!(ended["reason"], "user-stop");
}

/// Two runs asked for at once: the newer replaces the other, and no run is
/// left going that nobody can stop.
#[cfg(unix)]
#[tokio::test]
async fn two_live_runs_asked_for_at_once_leave_one_going() {
    let server = site("0.1.0", run_answer()).await;
    let m = live_machine(GOOD, &live_blinky());
    let served = m.serve(&server.uri());

    // The newer replaces the other: before it is built, or as soon as
    // sim86 has started it.
    let ((s1, _, a), (s2, _, b)) = tokio::join!(served.run(live_ask(60)), served.run(live_ask(60)));
    let mut answers = [(s1, a), (s2, b)];
    answers.sort_by_key(|(status, _)| *status);
    let [(going, live), (replaced, error)] = answers;
    assert_eq!((going, replaced), (200, 409), "{live} {error}");
    assert_eq!(error["error"]["code"], "replaced", "{error}");
    let id = live["live"].as_str().unwrap();
    assert_eq!(served.live_status(id).await, 200);
    served.live_post(id, "stop", json!({})).await;
    // Every run sim86 started is stopped.
    let starts = m.sim86_saw("live-starts").lines().count();
    assert!((1..=2).contains(&starts), "{starts}");
    assert!(
        sim86_says(&m, "got-stop", &vec!["stop"; starts].join("\n"), 10).await,
        "{starts} started: {:?}",
        std::fs::read_to_string(m.sim86_bin().join("got-stop"))
    );
}
