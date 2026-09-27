//! `ter new` against a fake xiao-generate on the PATH.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

/// A machine whose xiao-generate logs its arguments, one per line, and
/// exits with `$FAKE_EXIT`.
struct Machine {
    root: tempfile::TempDir,
}

const FAKE: &str = r#"#!/bin/sh
for a in "$@"; do echo "$a"; done >> "$FAKE/args"
case "$1" in --list-*) echo "Supported XIAO module variants:"; exit 0 ;; esac
echo "generated"
exit "${FAKE_EXIT:-0}"
"#;

impl Machine {
    fn new(with_generator: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        if with_generator {
            let path = root.path().join("bin/xiao-generate");
            std::fs::write(&path, FAKE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { root }
    }

    fn args(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("args"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn ter(&self, args: &[&str], exit: u8) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ter"))
            .arg("new")
            .args(args)
            .current_dir(self.root.path())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()),
            )
            .env("FAKE", self.root.path())
            .env("FAKE_EXIT", exit.to_string())
            .env("HOME", self.root.path())
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .output()
            .unwrap()
    }
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

#[test]
fn ters_flags_become_xiao_generates_and_extras_pass_through() {
    let m = Machine::new(true);
    let out = m.ter(
        &[
            "--board",
            "xiao-rp2040",
            "--hal",
            "embassy-rp",
            "--async",
            "--name",
            "blinky",
            "--",
            "--editor",
            "helix",
        ],
        0,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("generated"));
    assert_eq!(
        m.args(),
        [
            "--headless",
            "--chip",
            "xiao-rp2040",
            "--stack",
            "embassy-rp",
            "--framework",
            "embassy",
            "--name",
            "blinky",
            "--out",
            "blinky",
            "--editor",
            "helix",
        ]
    );
}

#[test]
fn a_curriculum_style_call_is_accepted() {
    // What exercise_sync passes xiao-generate, with ter's flag names.
    let m = Machine::new(true);
    let out = m.ter(
        &[
            "--headless",
            "--board",
            "xiao-esp32c3",
            "--hal",
            "esp-hal",
            "--logging",
            "log",
            "--name",
            "exercise",
            "--no-install",
            "--out",
            "/tmp/somewhere/exercise",
        ],
        0,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let args = m.args();
    assert_eq!(args.iter().filter(|a| *a == "--headless").count(), 1);
    assert!(args.ends_with(&[
        "--no-install".into(),
        "--out".into(),
        "/tmp/somewhere/exercise".into()
    ]));
}

#[test]
fn json_names_the_project_folder_and_keeps_the_generators_output_off_stdout() {
    let m = Machine::new(true);
    let out = m.ter(
        &["--board", "xiao-esp32c3", "--name", "blinky", "--json"],
        0,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["board"], "xiao-esp32c3");
    assert_eq!(
        PathBuf::from(v["dir"].as_str().unwrap()),
        m.root.path().join("blinky")
    );
    assert!(text(&out.stderr).contains("generated"));
}

#[test]
fn a_generator_failure_is_generate_failed() {
    let m = Machine::new(true);
    let out = m.ter(&["--board", "xiao-nope", "--name", "x"], 2);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("error[generate_failed]"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn without_xiao_generate_ter_says_to_install_it() {
    let m = Machine::new(false);
    let out = m.ter(&["--board", "xiao-esp32c3", "--name", "x"], 0);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("error[not_installed]"), "{err}");
    assert!(err.contains("ter install xiao-generate"), "{err}");
}

#[test]
fn board_and_name_are_required_except_for_listings() {
    let m = Machine::new(true);
    assert!(!m.ter(&["--board", "xiao-esp32c3"], 0).status.success());
    assert!(m.args().is_empty());

    let out = m.ter(&["--list-boards", "--json"], 0);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["listing"].as_str().unwrap().contains("XIAO"));

    m.ter(&["--list-templates", "--board", "xiao-rp2040"], 0);
    assert!(m.args().ends_with(&[
        "--list-templates".into(),
        "--chip".into(),
        "xiao-rp2040".into()
    ]));
}
