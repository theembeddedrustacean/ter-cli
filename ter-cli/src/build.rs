//! Building an exercise with cargo, the way `ter run` records it.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// `text` without terminal escape sequences (colour, hyperlinks).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_and_links_are_stripped() {
        let coloured = "\u{1b}[0m\u{1b}[1m\u{1b}[38;5;9merror[E0425]\u{1b}[0m: cannot find value";
        assert_eq!(strip_ansi(coloured), "error[E0425]: cannot find value");
        let link = "see \u{1b}]8;;https://doc.rust-lang.org\u{1b}\\docs\u{1b}]8;;\u{1b}\\ now";
        assert_eq!(strip_ansi(link), "see docs now");
        let bel = "\u{1b}]8;;x\u{7}a\u{1b}]8;;\u{7}";
        assert_eq!(strip_ansi(bel), "a");
        assert_eq!(strip_ansi("plain\n"), "plain\n");
    }

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
