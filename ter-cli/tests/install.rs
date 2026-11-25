//! `ter install` against fake `cargo` and `rustup` on the PATH: what it
//! installs, and that a second run installs nothing.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// A machine with a fake cargo and rustup. `cargo install <crate>` writes
/// a program that prints its version; rustup keeps its installed targets,
/// components and toolchains in files. Every call is logged.
struct Machine {
    root: tempfile::TempDir,
}

const CARGO: &str = r#"#!/bin/sh
echo "cargo $*" >> "$FAKE/log"
[ "$1" = install ] || exit 1
case "$2" in
  probe-rs-tools) bin=probe-rs ;;
  *) bin="$2" ;;
esac
printf '#!/bin/sh\necho "%s 9.1.0"\n' "$bin" > "$FAKE/bin/$bin"
chmod +x "$FAKE/bin/$bin"
"#;

const RUSTUP: &str = r#"#!/bin/sh
echo "rustup $*" >> "$FAKE/log"
touch "$FAKE/targets" "$FAKE/components"
case "$1 $2" in
  "toolchain list") cat "$FAKE/toolchains" ;;
  "toolchain install") echo "$3-x86_64-unknown-linux-gnu" >> "$FAKE/toolchains" ;;
  "target list") cat "$FAKE/targets" ;;
  "target add") echo "$3" >> "$FAKE/targets" ;;
  "component list") cat "$FAKE/components" ;;
  "component add") echo "$3" >> "$FAKE/components" ;;
  *) exit 1 ;;
esac
"#;

impl Machine {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let m = Self { root };
        std::fs::create_dir_all(m.bin()).unwrap();
        m.program("cargo", CARGO);
        m.program("rustup", RUSTUP);
        std::fs::write(
            m.root.path().join("toolchains"),
            "stable-x86_64-unknown-linux-gnu (default)\n",
        )
        .unwrap();
        m
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn program(&self, name: &str, script: &str) {
        let path = self.bin().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("log")).unwrap_or_default()
    }

    fn ter(&self, dir: &Path, args: &[&str]) -> Output {
        self.ter_with(dir, args, &[])
    }

    fn ter_with(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ter"))
            .args(["install"])
            .args(args)
            .current_dir(dir)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("FAKE", self.root.path())
            .env("HOME", self.root.path())
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .env("TER_KEYCHAIN", "off")
            .env("TER_CONFIG_DIR", self.root.path().join("config"))
            .env("TER_TOOLS_DIR", self.tools())
            .env_remove("SIM86_API_KEY")
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    /// ter's own tools folder on this machine.
    fn tools(&self) -> PathBuf {
        self.root.path().join("tools")
    }
}

/// Plays Python, the one on the PATH and, once `-m venv` has copied it,
/// the one of the environment: `--version`, `-m venv <dir>`, `-m pip
/// install ...` (after which the sim86 package is "there"), and ter's
/// helper, which it answers for. Every call is logged.
const PYTHON: &str = r#"#!/bin/sh
echo "python $*" >> "$FAKE/log"
case "$1" in
  --version) echo "Python ${FAKE_PYTHON_VERSION:-3.12.3}"; exit 0 ;;
  -m)
    case "$2" in
      venv) mkdir -p "$3/bin" && cp "$0" "$3/bin/python" ;;
      pip) touch "$FAKE/sim86-package" ;;
    esac
    exit 0 ;;
esac
[ -f "$1" ] || { echo "python: can't open file '$1'" >&2; exit 2; }
if [ ! -f "$FAKE/sim86-package" ]; then
  echo '{"helper": 1, "ok": false, "python": "3.12.3", "error": {"kind": "missing", "message": "the sim86 package is not in this Python"}}'
  exit 0
fi
case "$2" in
  version) echo '{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4"}' ;;
  balance)
    if [ -n "$SIM86_API_KEY" ]; then
      echo '{"helper": 1, "ok": true, "python": "3.12.3", "sim86": "0.1.4", "balance": {"credits": 87, "plan": "free"}}'
    else
      echo '{"helper": 1, "ok": false, "sim86": "0.1.4", "error": {"kind": "no_key", "message": "no API key: set SIM86_API_KEY, or run `sim86 login`"}}'
    fi ;;
esac
"#;

fn project(config: &str, toolchain: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
    std::fs::write(dir.path().join(".cargo/config.toml"), config).unwrap();
    if let Some(t) = toolchain {
        std::fs::write(dir.path().join("rust-toolchain.toml"), t).unwrap();
    }
    dir
}

fn c3() -> tempfile::TempDir {
    project(
        "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n\n[build]\ntarget = \"riscv32imc-unknown-none-elf\"\n",
        Some(
            "[toolchain]\nchannel = \"stable\"\ncomponents = [\"rust-src\"]\ntargets = [\"riscv32imc-unknown-none-elf\"]\n",
        ),
    )
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

fn installs(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| {
            l.starts_with("cargo install") || l.contains(" add ") || l.contains(" install ")
        })
        .collect()
}

#[test]
fn tools_and_targets_are_installed_once_and_the_second_run_is_a_no_op() {
    let m = Machine::new();
    let dir = c3();
    let first = m.ter(dir.path(), &["targets", "espflash", "probe-rs"]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_eq!(
        installs(&m.log()),
        [
            "rustup component add rust-src --toolchain stable",
            "rustup target add riscv32imc-unknown-none-elf --toolchain stable",
            "cargo install espflash --locked",
            "cargo install probe-rs-tools --locked",
        ]
    );
    let out = text(&first.stdout);
    assert!(out.contains("Installed espflash."), "{out}");
    assert!(
        out.contains("Rust toolchain stable is already installed"),
        "{out}"
    );

    let before = m.log();
    let second = m.ter(dir.path(), &["targets", "espflash", "probe-rs"]);
    assert!(second.status.success(), "{}", text(&second.stderr));
    assert_eq!(
        installs(&m.log()).len(),
        installs(&before).len(),
        "{}",
        m.log()
    );
    let out = text(&second.stdout);
    assert!(
        out.contains("espflash is already installed (9.1.0"),
        "{out}"
    );
    assert!(
        out.contains("Rust target riscv32imc-unknown-none-elf (stable) is already installed"),
        "{out}"
    );
    assert!(!out.contains("Installing"), "{out}");
}

#[test]
fn json_answers_one_document_with_what_was_done() {
    let m = Machine::new();
    let dir = c3();
    let out = m.ter(dir.path(), &["espflash", "--json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["tools"][0]["tool"], "espflash");
    assert_eq!(v["tools"][0]["items"][0]["state"], "installed");
    assert_eq!(
        v["tools"][0]["items"][0]["detail"],
        "cargo install espflash --locked"
    );

    let again = m.ter(dir.path(), &["espflash", "--json"]);
    let v: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(v["tools"][0]["items"][0]["state"], "present");
}

#[test]
fn a_folder_with_no_target_is_refused_before_anything_is_installed() {
    let m = Machine::new();
    let empty = tempfile::tempdir().unwrap();
    let out = m.ter(empty.path(), &["espflash", "targets"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("error[no_project]"), "{err}");
    assert!(m.log().is_empty(), "{}", m.log());
}

#[test]
fn dir_names_the_project_to_read() {
    let m = Machine::new();
    let dir = project("[build]\ntarget = \"thumbv7em-none-eabihf\"\n", None);
    let elsewhere = tempfile::tempdir().unwrap();
    let out = m.ter(
        elsewhere.path(),
        &["targets", "--dir", dir.path().to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        installs(&m.log()),
        ["rustup target add thumbv7em-none-eabihf"]
    );
}

#[test]
fn a_failing_install_stops_with_install_failed() {
    let m = Machine::new();
    m.program(
        "cargo",
        "#!/bin/sh\necho \"cargo $*\" >> \"$FAKE/log\"\nexit 101\n",
    );
    let dir = c3();
    let out = m.ter(dir.path(), &["espflash", "probe-rs"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.contains("error[install_failed]: `cargo install espflash --locked` failed"),
        "{err}"
    );
    assert!(!m.log().contains("probe-rs-tools"), "{}", m.log());
}

#[test]
fn unreleased_tools_are_refused() {
    let m = Machine::new();
    let dir = c3();
    let out = m.ter(dir.path(), &["telemetry-firmware"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("error[not_available]"),
        "{}",
        text(&out.stderr)
    );
    assert!(m.log().is_empty());
}

#[test]
fn sim86_goes_into_ters_own_python_environment_with_the_helper_beside_it() {
    let m = Machine::new();
    m.program("python3", PYTHON);
    let dir = tempfile::tempdir().unwrap();
    let home = m.tools().join("sim86");

    let first = m.ter(dir.path(), &["sim86"]);
    assert!(first.status.success(), "{}", text(&first.stderr));
    let venv = home.join("venv");
    assert_eq!(
        m.log()
            .lines()
            .filter(|l| l.contains(" -m "))
            .collect::<Vec<_>>(),
        [
            format!("python -m venv {}", venv.display()),
            "python -m pip install --upgrade --quiet sim86>=0.1.4,<0.3".to_string(),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(home.join("ter_sim86.py")).unwrap(),
        ter_telemetry::sim86::HELPER
    );
    let out = text(&first.stdout);
    assert!(
        out.contains(&format!("Installed sim86 0.1.4 in {}.", home.display())),
        "{out}"
    );
    // No key yet: the install is done, and the next step is named.
    assert!(
        out.contains(&format!(
            "Sign in with `{} login`, or set SIM86_API_KEY.",
            venv.join("bin/sim86").display()
        )),
        "{out}"
    );

    // Again, with a key: nothing is installed, and the account is shown.
    let before = m.log();
    let key = "sk-sim86-never-shown";
    let again = m.ter_with(dir.path(), &["sim86", "--json"], &[("SIM86_API_KEY", key)]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert!(!m.log()[before.len()..].contains(" -m "), "{}", m.log());
    let v: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(
        v["tools"][0],
        serde_json::json!({
            "tool": "sim86",
            "path": home,
            "installed": false,
            "version": "0.1.4",
            "python": "3.12.3",
            "key": "accepted",
            "plan": "free",
            "credits": 87,
        })
    );
    assert!(!text(&again.stdout).contains(key) && !text(&again.stderr).contains(key));
    let said = m.ter_with(dir.path(), &["sim86"], &[("SIM86_API_KEY", key)]);
    assert!(
        text(&said.stdout).contains(
            "Your sim86 key is accepted: plan free, 87 credits (one a simulated minute). `ter run --sim sim86` is ready."
        ),
        "{}",
        text(&said.stdout)
    );

    // A helper someone changed is put back.
    std::fs::write(home.join("ter_sim86.py"), "print('edited')\n").unwrap();
    assert!(m.ter(dir.path(), &["sim86"]).status.success());
    assert_eq!(
        std::fs::read_to_string(home.join("ter_sim86.py")).unwrap(),
        ter_telemetry::sim86::HELPER
    );
}

#[test]
fn sim86_needs_a_python_from_3_9() {
    let m = Machine::new();
    m.program("python3", PYTHON);
    m.program("python", PYTHON);
    let dir = tempfile::tempdir().unwrap();
    let out = m.ter_with(dir.path(), &["sim86"], &[("FAKE_PYTHON_VERSION", "3.8.19")]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("error[install_failed]"), "{err}");
    assert!(
        err.contains("sim86 needs Python 3.9 or newer, and this machine has Python 3.8.19"),
        "{err}"
    );
    assert!(!m.tools().join("sim86/venv").exists());
}
