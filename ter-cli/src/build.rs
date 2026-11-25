//! Building an exercise with cargo, the way `ter run` records it.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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

/// Stops a build from elsewhere: cargo and every process it started
/// (rustc, build scripts, the linker), at once. A build asked to stop
/// before cargo starts never starts it.
#[derive(Debug, Clone, Default)]
pub struct Abort(Arc<Mutex<Aborting>>);

#[derive(Debug, Default)]
struct Aborting {
    aborted: bool,
    /// cargo's process id while it runs: its process group's too.
    cargo: Option<u32>,
}

impl Abort {
    pub fn abort(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.aborted = true;
        if let Some(cargo) = state.cargo {
            kill_tree(cargo);
        }
    }

    pub fn aborted(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).aborted
    }

    /// Note cargo as started, or stop it now if the build was stopped
    /// while it started.
    fn started(&self, cargo: u32) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.aborted {
            kill_tree(cargo);
        }
        state.cargo = Some(cargo);
    }

    /// Note cargo as gone. A stopped build's group is stopped once more,
    /// for any process cargo left behind.
    fn ended(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cargo) = state.cargo.take()
            && state.aborted
        {
            kill_tree(cargo);
        }
    }
}

/// Stop `cargo` and everything in its process group, which it leads.
#[cfg(unix)]
fn kill_tree(cargo: u32) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(group) = i32::try_from(cargo).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

/// Stop `cargo` and every process under it.
#[cfg(windows)]
fn kill_tree(cargo: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &cargo.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Build the exercise in `dir` with `env` (the course's shared cache).
/// With `echo`, cargo's output also goes to stderr as it comes, in colour
/// on a terminal. With `abort`, cargo runs in a process group of its own,
/// so stopping the build stops everything it started; without, Ctrl-C on
/// a terminal reaches it as before.
pub fn cargo_build(
    dir: &Path,
    env: Vec<(&'static str, OsString)>,
    echo: bool,
    abort: Option<&Abort>,
) -> Result<Build, CliError> {
    let colour = if echo && std::io::stderr().is_terminal() {
        "always"
    } else {
        "never"
    };
    let started = Instant::now();
    // Plain `cargo`, through rustup, so the exercise's toolchain applies.
    let mut cargo = Command::new("cargo");
    #[cfg(unix)]
    if abort.is_some() {
        std::os::unix::process::CommandExt::process_group(&mut cargo, 0);
    }
    let mut child = cargo
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
    if let Some(abort) = abort {
        abort.started(child.id());
    }

    let stderr = child.stderr.take().expect("piped");
    let log = std::thread::spawn(move || read_log(stderr, echo));
    let stdout = child.stdout.take().expect("piped");
    let elf = find_executable(BufReader::new(stdout));
    let log = log.join().expect("log reader does not panic");
    let status = child.wait();
    if let Some(abort) = abort {
        abort.ended();
    }
    let status =
        status.map_err(|e| CliError::new("build_error", format!("cargo did not finish: {e}")))?;
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
