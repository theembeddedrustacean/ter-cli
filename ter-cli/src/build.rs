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

/// The file in a course's build output naming the exercise folder built
/// there last.
pub const LAST_BUILT: &str = "ter-last-built";

/// The lock file in a course's build output, held around a build and the
/// copy of its program.
pub const BUILD_LOCK: &str = "ter-build.lock";

/// Where in a run's folder the program it ran is kept.
pub const PROGRAM_DIR: &str = "program";

/// Build the exercise in `dir` with `env`, into the course's shared build
/// output `shared`, and keep the program in `keep`: the program cargo
/// reported for this build, copied before any other build in the course
/// can replace it.
///
/// Every exercise of a course is a package named `exercise` building into
/// the same files, so cargo can call an exercise fresh while those files
/// hold the program of another one. When another folder was built in
/// `shared` last, the exercise's own package is cleaned first; its
/// dependencies stay built. One build per course at a time.
///
/// With `echo`, cargo's output also goes to stderr as it comes, in colour
/// on a terminal. With `abort`, cargo runs in a process group of its own,
/// so stopping the build stops everything it started; without, Ctrl-C on
/// a terminal reaches it as before.
pub fn cargo_build(
    dir: &Path,
    env: Vec<(&'static str, OsString)>,
    shared: &Path,
    keep: &Path,
    echo: bool,
    abort: Option<&Abort>,
) -> Result<Build, CliError> {
    let started = Instant::now();
    let Some(_lock) = lock_course(shared, echo, abort)? else {
        return Ok(Build {
            ok: false,
            log: String::new(),
            elf: None,
            duration: started.elapsed(),
        });
    };
    let here = folder_id(dir);
    if !built_last(shared, &here)
        && let Some(log) = clean_package(dir, &env)?
    {
        return Ok(Build {
            ok: false,
            log,
            elf: None,
            duration: started.elapsed(),
        });
    }
    let mut build = run_cargo(dir, env, echo, abort)?;
    // Whatever cargo got to write is this exercise's now.
    mark_built(shared, &here)?;
    if let Some(elf) = build.elf.take() {
        build.elf = Some(keep_program(&elf, keep)?);
    }
    build.duration = started.elapsed();
    Ok(build)
}

/// Hold the course's build lock, waiting for another build to finish.
/// `None` when the build was stopped while it waited.
fn lock_course(
    shared: &Path,
    echo: bool,
    abort: Option<&Abort>,
) -> Result<Option<std::fs::File>, CliError> {
    let failed = |e: std::io::Error| {
        CliError::new(
            "build_error",
            format!("Could not lock {}: {e}", shared.join(BUILD_LOCK).display()),
        )
    };
    std::fs::create_dir_all(shared).map_err(failed)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(shared.join(BUILD_LOCK))
        .map_err(failed)?;
    let mut said = false;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(Some(lock)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(failed(e)),
        }
        if abort.is_some_and(Abort::aborted) {
            return Ok(None);
        }
        if echo && !said {
            eprintln!("Waiting for another build in this course to finish");
            said = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How the course's build output names an exercise folder.
fn folder_id(dir: &Path) -> String {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.to_string_lossy().into_owned()
}

/// Whether the folder `here` was built last in `shared`. No record, as
/// before ter kept one, is someone else.
fn built_last(shared: &Path, here: &str) -> bool {
    std::fs::read_to_string(shared.join(LAST_BUILT)).is_ok_and(|last| last.trim_end() == here)
}

fn mark_built(shared: &Path, here: &str) -> Result<(), CliError> {
    let path = shared.join(LAST_BUILT);
    std::fs::write(&path, format!("{here}\n")).map_err(|e| {
        CliError::new(
            "build_error",
            format!("Could not write {}: {e}", path.display()),
        )
    })
}

/// The package name in the exercise's Cargo.toml.
fn package_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let manifest: toml::Table = text.parse().ok()?;
    Some(manifest.get("package")?.get("name")?.as_str()?.to_string())
}

/// Clean the exercise's own package from the shared build output, for the
/// target and profile its build uses; the log of a clean that failed. A
/// manifest with no package name is left to cargo build to report.
fn clean_package(dir: &Path, env: &[(&'static str, OsString)]) -> Result<Option<String>, CliError> {
    let Some(name) = package_name(dir) else {
        return Ok(None);
    };
    let out = Command::new("cargo")
        .args(["clean", "--color=never", "-p", &name])
        .current_dir(dir)
        .envs(env.iter().cloned())
        .env_remove("RUSTUP_TOOLCHAIN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            CliError::new(
                "build_error",
                format!("Could not start cargo: {e}. Is Rust installed (https://rustup.rs)?"),
            )
        })?;
    if out.status.success() {
        return Ok(None);
    }
    Ok(Some(format!(
        "Another exercise was built in this course last, and cargo could not clean {name}'s build of it:\n{}",
        strip_ansi(&String::from_utf8_lossy(&out.stderr))
    )))
}

/// Copy the program to `keep`, under the name cargo gave it.
fn keep_program(elf: &Path, keep: &Path) -> Result<PathBuf, CliError> {
    let to = keep.join(elf.file_name().unwrap_or(elf.as_os_str()));
    std::fs::create_dir_all(keep)
        .and_then(|()| std::fs::copy(elf, &to))
        .map_err(|e| {
            CliError::new(
                "build_error",
                format!(
                    "Could not copy the program {} to {}: {e}",
                    elf.display(),
                    to.display()
                ),
            )
        })?;
    Ok(to)
}

fn run_cargo(
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

    #[test]
    fn the_course_remembers_the_folder_built_last() {
        let shared = tempfile::tempdir().unwrap();
        let (a, b) = ("/courses/c/a", "/courses/c/b");
        assert!(!built_last(shared.path(), a), "no record is someone else");
        mark_built(shared.path(), a).unwrap();
        assert!(built_last(shared.path(), a));
        assert!(!built_last(shared.path(), b));
        mark_built(shared.path(), b).unwrap();
        assert!(!built_last(shared.path(), a));
        assert_eq!(
            std::fs::read_to_string(shared.path().join(LAST_BUILT)).unwrap(),
            "/courses/c/b\n"
        );
    }

    #[test]
    fn the_package_name_comes_from_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(package_name(dir.path()), None);
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        assert_eq!(package_name(dir.path()), None);
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"exercise\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(package_name(dir.path()).as_deref(), Some("exercise"));
    }

    /// A course with exercises `names`, each a package named `exercise`
    /// that prints its own name.
    fn course(names: &[&str]) -> tempfile::TempDir {
        let course = tempfile::tempdir().unwrap();
        for name in names {
            let dir = course.path().join(name);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"exercise\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("src/main.rs"),
                format!("fn main() {{\n    println!(\"{name}\");\n}}\n"),
            )
            .unwrap();
        }
        course
    }

    fn build_in(course: &Path, name: &str, run: u32) -> Build {
        let shared = course.join(".target");
        let env = vec![("CARGO_TARGET_DIR", shared.clone().into_os_string())];
        let keep = course.join(name).join(format!(".runs/{run}/program"));
        let build = cargo_build(&course.join(name), env, &shared, &keep, false, None).unwrap();
        assert!(build.ok, "{}", build.log);
        build
    }

    fn prints(build: &Build) -> String {
        let out = Command::new(build.elf.as_ref().unwrap()).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn the_same_exercise_again_is_not_rebuilt() {
        let course = course(&["a"]);
        let first = build_in(course.path(), "a", 1);
        assert!(first.log.contains("Compiling exercise"), "{}", first.log);
        let again = build_in(course.path(), "a", 2);
        assert!(!again.log.contains("Compiling"), "{}", again.log);
        assert_eq!(prints(&again), "a");
        assert_eq!(
            again.elf.as_deref(),
            Some(course.path().join("a/.runs/2/program/exercise").as_path()),
            "the program is kept in the run's folder"
        );
    }

    #[test]
    fn an_exercise_built_after_another_is_rebuilt_alone() {
        let course = course(&["a", "b"]);
        build_in(course.path(), "a", 1);
        let b = build_in(course.path(), "b", 1);
        assert_eq!(prints(&b), "b");
        let a = build_in(course.path(), "a", 2);
        assert!(a.log.contains("Compiling exercise"), "{}", a.log);
        assert_eq!(prints(&a), "a");
        assert_eq!(prints(&b), "b", "b's kept program is still b's");
    }

    #[test]
    fn two_exercises_built_at_once_each_get_their_own_program() {
        let course = course(&["a", "b"]);
        let root = course.path().to_path_buf();
        std::thread::scope(|s| {
            let builds: Vec<_> = ["a", "b", "a", "b"]
                .into_iter()
                .enumerate()
                .map(|(run, name)| {
                    let root = &root;
                    s.spawn(move || (name, build_in(root, name, run as u32)))
                })
                .collect();
            for build in builds {
                let (name, build) = build.join().unwrap();
                assert_eq!(prints(&build), name);
            }
        });
    }
}
