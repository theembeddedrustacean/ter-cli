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
            .output()
            .unwrap()
    }
}

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
    for tool in ["sim86", "telemetry-firmware"] {
        let out = m.ter(dir.path(), &[tool]);
        assert!(!out.status.success());
        assert!(
            text(&out.stderr).contains("error[not_available]"),
            "{}",
            text(&out.stderr)
        );
    }
    assert!(m.log().is_empty());
}
