//! Building an exercise with cargo, the way `ter run` records it.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ter_telemetry::strip_ansi;

use crate::output::CliError;

/// One `cargo build` of an exercise.
#[derive(Debug)]
pub struct Build {
    pub ok: bool,
    /// Cargo's human output (progress and diagnostics), without colour codes.
    pub log: String,
    /// The program cargo built, when it built one.
    pub elf: Option<PathBuf>,
    pub duration: Duration,
}

/// Build the exercise in `dir` with `env` (the course's shared cache).
/// With `echo`, cargo's output also goes to stderr as it comes, in colour
/// on a terminal.
pub fn cargo_build(
    dir: &Path,
    env: Vec<(&'static str, OsString)>,
    echo: bool,
) -> Result<Build, CliError> {
    let colour = if echo && std::io::stderr().is_terminal() {
        "always"
    } else {
        "never"
    };
    let started = Instant::now();
    // Plain `cargo`, through rustup, so the exercise's toolchain applies.
    let mut child = Command::new("cargo")
        .args(["build", "--message-format=json-render-diagnostics"])
        .arg(format!("--color={colour}"))
        .current_dir(dir)
        .envs(env)
        // The exercise's rust-toolchain.toml picks the toolchain, whatever
        // toolchain ter itself was started under.
        .env_remove("RUSTUP_TOOLCHAIN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            CliError::new(
                "build_error",
                format!("Could not start cargo: {e}. Is Rust installed (https://rustup.rs)?"),
            )
        })?;

    let stderr = child.stderr.take().expect("piped");
    let log = std::thread::spawn(move || read_log(stderr, echo));
    let stdout = child.stdout.take().expect("piped");
    let elf = find_executable(BufReader::new(stdout));
    let log = log.join().expect("log reader does not panic");
    let status = child
        .wait()
        .map_err(|e| CliError::new("build_error", format!("cargo did not finish: {e}")))?;
    Ok(Build {
        ok: status.success(),
        log,
        elf: elf.filter(|_| status.success()),
        duration: started.elapsed(),
    })
}

fn read_log(stream: impl Read, echo: bool) -> String {
    let mut reader = BufReader::new(stream);
    let mut log = String::new();
    let mut line = Vec::new();
    let mut out = std::io::stderr();
    while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
        if echo {
            let _ = out.write_all(&line);
        }
        log.push_str(&strip_ansi(&String::from_utf8_lossy(&line)));
        line.clear();
    }
    log
}

/// The first binary among cargo's JSON messages. A scaffold builds one
/// program; `cargo run` would refuse a package with several anyway.
fn find_executable(stdout: impl BufRead) -> Option<PathBuf> {
    let mut found = None;
    for line in stdout.lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if found.is_none()
            && msg["reason"] == "compiler-artifact"
            && let Some(exe) = msg["executable"].as_str()
        {
            found = Some(PathBuf::from(exe));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_binary_is_the_program() {
        let messages = [
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"]},"executable":null}"#,
            "not json",
            r#"{"reason":"compiler-artifact","target":{"kind":["bin"]},"executable":"/t/debug/exercise"}"#,
            r#"{"reason":"build-finished","success":true}"#,
        ]
        .join("\n");
        assert_eq!(
            find_executable(messages.as_bytes()),
            Some(PathBuf::from("/t/debug/exercise"))
        );
        assert_eq!(find_executable(&b""[..]), None);
    }
}
