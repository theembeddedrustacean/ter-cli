//! `ter install`: the tools ter and the exercises use.
//!
//! `targets` (the Rust target, toolchain and components of the project in
//! the current folder), `xiao-generate` (which `ter new` runs), `espflash`,
//! `probe-rs` and the UF2 runners `elf2uf2-rs` and `uf2deploy` (which
//! `cargo run` uses; `ter run` flashes UF2 boards itself) are installed
//! with rustup and cargo; `wokwi-cli` is
//! Wokwi's command line simulator, downloaded, with the learner's own Wokwi
//! token; `sim86` is the sim86 simulator's Python package, installed with
//! pip into a Python environment of ter's own, with ter's helper beside it.
//!
//! sim86 runs on the learner's own sim86 account too. Its key is the
//! package's to keep (`sim86 login`), or comes from `SIM86_API_KEY`; ter
//! stores no copy.
//!
//! TER holds no Wokwi account for anyone: each learner creates a CI token
//! on their own Wokwi account, and ter keeps it like the TER token (the
//! keychain, else a file only they can read). Running any of it twice is
//! safe: what is already there is left alone.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use ter_gen::install::{self as plan, Cmd, Item, State, Tool};

use crate::output::{CliError, print_json};
use crate::sim::{self, TOKEN_ENV, TOKEN_PAGE, WOKWI_CLI};
use crate::token_store::TokenStore;
use ter_telemetry::sim86 as sim86lib;

/// What `ter install` takes, in the order it installs them.
pub const TOOLS: [&str; 9] = [
    "targets",
    "xiao-generate",
    "espflash",
    "probe-rs",
    "elf2uf2-rs",
    "uf2deploy",
    "wokwi-cli",
    "sim86",
    "telemetry-firmware",
];

#[derive(Serialize)]
#[serde(untagged)]
enum Report {
    Planned { tool: String, items: Vec<Done> },
    Wokwi(Installed),
    Sim86(Sim86Installed),
}

#[derive(Serialize)]
struct Done {
    what: String,
    /// `present` (left alone) or `installed` (installed now).
    state: &'static str,
    /// What was found, or the commands that installed it.
    detail: String,
}

pub async fn run(
    tools: &[String],
    dir: Option<PathBuf>,
    token_stdin: bool,
    json: bool,
) -> Result<(), CliError> {
    for tool in tools {
        if tool == "telemetry-firmware" {
            return Err(CliError::new(
                "not_available",
                format!("`{tool}` cannot be installed yet: it is not released."),
            ));
        }
    }
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()
            .map_err(|e| CliError::new("io_error", format!("No current folder: {e}")))?,
    };
    // Read the project before installing anything, so a wrong folder fails
    // with nothing done.
    let project = if tools.iter().any(|t| t == "targets") {
        Some(plan::read_project(&dir).map_err(|m| {
            CliError::new(
                "no_project",
                format!(
                    "{m} Run `ter install targets` in an exercise or project folder, or pass --dir."
                ),
            )
        })?)
    } else {
        None
    };

    let mut reports = Vec::new();
    let mut seen = Vec::new();
    for name in TOOLS {
        if !tools.iter().any(|t| t == name) || seen.contains(&name) {
            continue;
        }
        seen.push(name);
        let tool = match name {
            "targets" => Tool::Targets,
            "espflash" => Tool::Espflash,
            "probe-rs" => Tool::ProbeRs,
            "elf2uf2-rs" => Tool::Elf2uf2Rs,
            "uf2deploy" => Tool::Uf2deploy,
            "xiao-generate" => Tool::XiaoGenerate,
            "sim86" => {
                reports.push(Report::Sim86(sim86(json)?));
                continue;
            }
            _ => {
                reports.push(Report::Wokwi(wokwi(token_stdin, json).await?));
                continue;
            }
        };
        let items = plan::plan(&plan::System, tool, project.as_ref());
        let mut done = Vec::new();
        for item in items {
            done.push(apply(item, &dir, json)?);
        }
        reports.push(Report::Planned {
            tool: name.to_string(),
            items: done,
        });
    }
    if json {
        print_json(&serde_json::json!({ "tools": reports }));
    }
    Ok(())
}

/// Install one item, or say it is already there.
fn apply(item: Item, dir: &Path, json: bool) -> Result<Done, CliError> {
    match item.state {
        State::Present(found) => {
            if !json {
                println!("{} is already installed ({found}).", item.what);
            }
            Ok(Done {
                what: item.what,
                state: "present",
                detail: found,
            })
        }
        State::Manual(what_to_do) => Err(CliError::new("install_failed", what_to_do)),
        State::Install { why, run } => {
            if !json {
                println!("Installing {} ({why}).", item.what);
            }
            for cmd in &run {
                execute(cmd, dir, json)?;
            }
            if !json {
                println!("Installed {}.", item.what);
            }
            if let Some(program) = run
                .iter()
                .find(|c| c.program == "cargo")
                .and_then(|c| c.args.get(1))
                .map(|krate| {
                    if krate == "probe-rs-tools" {
                        "probe-rs"
                    } else {
                        krate
                    }
                })
                && ter_flash::find_program(program).is_none()
            {
                eprintln!(
                    "{program} is installed in cargo's bin folder, which is not on your PATH yet. Add it (usually ~/.cargo/bin) so ter can find it."
                );
            }
            Ok(Done {
                what: item.what,
                state: "installed",
                detail: run
                    .iter()
                    .map(Cmd::to_string)
                    .collect::<Vec<_>>()
                    .join(" && "),
            })
        }
    }
}

/// Run one install command in the project folder. With `--json` its
/// output goes to stderr, so stdout stays one JSON document.
fn execute(cmd: &Cmd, dir: &Path, json: bool) -> Result<(), CliError> {
    let fail = |m: String| CliError::new("install_failed", m);
    let program = plan::find_program(&cmd.program).ok_or_else(|| {
        fail(format!(
            "`{cmd}` needs {}, which is not installed.",
            cmd.program
        ))
    })?;
    let mut command = Command::new(program);
    command
        .args(&cmd.args)
        .current_dir(dir)
        .stdin(Stdio::null());
    if json {
        command.stdout(std::io::stderr());
    }
    let status = command
        .status()
        .map_err(|e| fail(format!("Could not run `{cmd}`: {e}")))?;
    if !status.success() {
        return Err(fail(format!("`{cmd}` failed ({status}).")));
    }
    Ok(())
}

/// What `ter install sim86` did and found.
#[derive(Serialize)]
struct Sim86Installed {
    tool: &'static str,
    /// The folder with the Python environment and ter's helper.
    path: PathBuf,
    /// Installed or updated now, rather than already there.
    installed: bool,
    /// The sim86 package's version.
    version: String,
    python: String,
    /// `accepted`, `none`, `refused` or `unknown` (sim86 did not answer).
    key: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credits: Option<i64>,
}

/// The oldest Python the sim86 package runs on.
const MIN_PYTHON: (u32, u32) = (3, 9);

/// `Python 3.12.3` as `(3, 12)`.
fn python_version(said: &str) -> Option<(u32, u32)> {
    let mut numbers = said.trim().strip_prefix("Python ")?.split('.');
    Some((numbers.next()?.parse().ok()?, numbers.next()?.parse().ok()?))
}

/// A Python new enough for sim86, from the PATH.
fn find_python() -> Result<PathBuf, CliError> {
    let mut too_old = None;
    for name in ["python3", "python"] {
        let Some(path) = plan::find_program(name) else {
            continue;
        };
        let said = Command::new(&path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_default();
        match python_version(&said) {
            Some(v) if v >= MIN_PYTHON => return Ok(path),
            Some(_) => too_old = Some(said.trim().to_string()),
            None => {}
        }
    }
    let (major, minor) = MIN_PYTHON;
    Err(CliError::new(
        "install_failed",
        match too_old {
            Some(found) => format!(
                "sim86 needs Python {major}.{minor} or newer, and this machine has {found}. Install a newer Python, then run `ter install sim86` again."
            ),
            None => format!(
                "sim86 is a Python package and no Python is on the PATH. Install Python {major}.{minor} or newer (python.org, or your system's package manager), then run `ter install sim86` again."
            ),
        },
    ))
}

/// Whether an installed package version is one [`sim86lib::PACKAGE`] asks
/// for: at least the tested one, and before the next minor.
fn package_fits(version: &str) -> bool {
    plan::at_least(version, sim86lib::TESTED)
        && version.split('.').take(2).collect::<Vec<_>>()
            == sim86lib::TESTED.split('.').take(2).collect::<Vec<_>>()
}

/// Run one step of the install; with `--json` its output goes to stderr.
fn step(command: &mut Command, what: &str, hint: &str, json: bool) -> Result<(), CliError> {
    command.stdin(Stdio::null());
    if json {
        command.stdout(std::io::stderr());
    }
    let done = command.status().is_ok_and(|s| s.success());
    if done {
        Ok(())
    } else {
        Err(CliError::new(
            "install_failed",
            format!("Could not {what}.{hint}"),
        ))
    }
}

/// The Python environment with the sim86 package, ter's helper beside it,
/// and a look at the learner's sim86 account.
fn sim86(json: bool) -> Result<Sim86Installed, CliError> {
    let fail = |m: String| CliError::new("install_failed", m);
    let home = crate::sim86::home()?;
    let python = crate::sim86::python_in(&home);
    let helper = sim86lib::Helper {
        python: python.clone(),
        script: home.join(sim86lib::HELPER_FILE),
        key: None,
    };
    let write_helper = || {
        std::fs::create_dir_all(&home)
            .and_then(|()| crate::sim86::write_helper(&helper))
            .map_err(|e| fail(format!("Could not write {}: {e}", helper.script.display())))
    };
    let present = python.is_file()
        && write_helper().is_ok()
        && helper
            .version()
            .is_ok_and(|v| v.sim86.as_deref().is_some_and(package_fits));
    if !present {
        let system = find_python()?;
        if !json {
            println!("Installing sim86 in {}.", home.display());
        }
        std::fs::create_dir_all(&home)
            .map_err(|e| fail(format!("Could not create {}: {e}", home.display())))?;
        if !python.is_file() {
            step(
                Command::new(&system)
                    .args(["-m", "venv"])
                    .arg(home.join("venv")),
                "create a Python environment for sim86",
                " On Debian and Ubuntu, Python's venv module is its own package: `sudo apt install python3-venv`.",
                json,
            )?;
        }
        step(
            Command::new(&python)
                .args(["-m", "pip", "install", "--upgrade", "--quiet"])
                .arg(sim86lib::PACKAGE)
                .env("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
            "install the sim86 package with pip",
            " It is downloaded from PyPI: check the network, then run `ter install sim86` again.",
            json,
        )?;
        write_helper()?;
    }
    let found = helper.version().map_err(|e| fail(e.explain()))?;
    let version = found.sim86.unwrap_or_default();
    if !package_fits(&version) {
        return Err(fail(format!(
            "pip installed sim86 {version}, and ter needs {}.",
            sim86lib::PACKAGE
        )));
    }
    let asked = sim86lib::Helper {
        key: crate::sim86::installed().and_then(|h| h.key),
        ..helper.clone()
    }
    .balance();
    let (key, balance) = match asked {
        Ok(balance) => ("accepted", Some(balance)),
        Err(e) if e.kind == sim86lib::FailureKind::NoKey => ("none", None),
        Err(e) if e.kind == sim86lib::FailureKind::Auth => ("refused", None),
        Err(_) => ("unknown", None),
    };
    if !json {
        if present {
            println!(
                "sim86 {version} is already installed in {}.",
                home.display()
            );
        } else {
            println!("Installed sim86 {version} in {}.", home.display());
        }
        let login = crate::sim86::command_in(&home);
        match (key, &balance) {
            (_, Some(b)) => println!(
                "Your sim86 key is accepted: plan {}, {} credits (one a simulated minute). `ter run --sim sim86` is ready.",
                b.plan, b.credits
            ),
            ("none", _) => println!(
                "sim86 runs on your own sim86 account, and this machine has no key for it yet. Sign in with `{} login`, or set {}.",
                login.display(),
                sim86lib::KEY_ENV
            ),
            ("refused", _) => println!(
                "sim86 refused the key on this machine. Sign in again with `{} login`, or put a new key in {}.",
                login.display(),
                sim86lib::KEY_ENV
            ),
            _ => {
                println!("sim86 did not say whether your key is accepted; `ter doctor` asks again.")
            }
        }
    }
    Ok(Sim86Installed {
        tool: "sim86",
        path: home,
        installed: !present,
        version,
        python: found.python.unwrap_or_default(),
        key,
        plan: balance.as_ref().map(|b| b.plan.clone()),
        credits: balance.as_ref().map(|b| b.credits),
    })
}

const RELEASES: &str = "https://github.com/wokwi/wokwi-cli/releases/latest/download";

#[derive(Serialize)]
struct Installed {
    tool: &'static str,
    path: PathBuf,
    /// Downloaded now, rather than already there.
    downloaded: bool,
    /// Where the token is: `keychain`, `file`, `WOKWI_CLI_TOKEN`.
    token: &'static str,
    token_stored_now: bool,
}

async fn wokwi(token_stdin: bool, json: bool) -> Result<Installed, CliError> {
    let (path, downloaded) = match sim::find_cli() {
        Some(p) => (p, false),
        None => (download().await?, true),
    };
    if !json {
        if downloaded {
            println!("Installed wokwi-cli at {}.", path.display());
        } else {
            println!("wokwi-cli is already installed at {}.", path.display());
        }
    }

    let store = TokenStore::open_wokwi()?;
    let stored = store.load()?;
    let (token_source, stored_now) = if token_stdin {
        let token = read_line(std::io::stdin().lock())?;
        (save(&store, &token)?, true)
    } else if let Some((_, location)) = &stored {
        (location.source(), false)
    } else if std::env::var(TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty()) {
        (TOKEN_ENV, false)
    } else if std::io::stdin().is_terminal() && !json {
        println!(
            "Simulation runs on your own Wokwi account. Sign in at {TOKEN_PAGE} (a free account works), create a CI token there and paste it here."
        );
        let token = rpassword::prompt_password("Wokwi token: ")
            .map_err(|e| CliError::new("io_error", format!("Could not read the token: {e}")))?;
        (save(&store, &token)?, true)
    } else {
        return Err(CliError::new(
            "no_token",
            format!(
                "No Wokwi token yet. Create a CI token at {TOKEN_PAGE} and pass it on standard input: `ter install wokwi-cli --token-stdin`."
            ),
        ));
    };

    if json {
        // The caller prints every tool's answer as one document.
    } else if stored_now {
        println!("Your Wokwi token is stored ({token_source}). `ter run --sim` is ready.");
    } else {
        println!("Using the Wokwi token from {token_source}. `ter run --sim` is ready.");
    }
    Ok(Installed {
        tool: WOKWI_CLI,
        path,
        downloaded,
        token: token_source,
        token_stored_now: stored_now,
    })
}

fn read_line(input: impl BufRead) -> Result<String, CliError> {
    let line = input
        .lines()
        .next()
        .transpose()
        .map_err(|e| CliError::new("io_error", format!("Could not read the token: {e}")))?
        .unwrap_or_default();
    Ok(line.trim().to_string())
}

fn save(store: &TokenStore, token: &str) -> Result<&'static str, CliError> {
    let token = token.trim();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err(CliError::new(
            "no_token",
            "That is not a Wokwi token: it is empty or has spaces in it.",
        ));
    }
    let saved = store.save(token)?;
    if let Some(why) = &saved.keychain_error {
        eprintln!("No keychain here ({why}); the token is in a file only you can read.");
    }
    Ok(saved.location.source())
}

/// The release asset for this machine.
fn asset() -> Result<&'static str, CliError> {
    use std::env::consts::{ARCH, OS};
    Ok(match (OS, ARCH) {
        ("linux", "x86_64") => "wokwi-cli-linuxstatic-x64",
        ("linux", "aarch64") => "wokwi-cli-linuxstatic-arm64",
        ("macos", "x86_64") => "wokwi-cli-macos-x64",
        ("macos", "aarch64") => "wokwi-cli-macos-arm64",
        ("windows", "x86_64") => "wokwi-cli-win-x64.exe",
        _ => {
            return Err(CliError::new(
                "install_failed",
                format!("Wokwi publishes no wokwi-cli for {OS} on {ARCH}."),
            ));
        }
    })
}

/// Download the latest `wokwi-cli` to where Wokwi's own installer puts it.
async fn download() -> Result<PathBuf, CliError> {
    let fail = |m: String| CliError::new("install_failed", m);
    let asset = asset()?;
    let dir = sim::home_bin().ok_or_else(|| fail("No home folder to install into.".into()))?;
    let url = format!("{RELEASES}/{asset}");
    let bytes = reqwest::get(&url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| fail(format!("Could not download {url}: {e}")))?
        .bytes()
        .await
        .map_err(|e| fail(format!("Could not download {url}: {e}")))?;
    let path = dir.join(format!("{WOKWI_CLI}{}", std::env::consts::EXE_SUFFIX));
    write_executable(&path, &bytes)
        .map_err(|e| fail(format!("Could not write {}: {e}", path.display())))?;
    Ok(path)
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    let mut file = std::fs::File::create(&partial)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    drop(file);
    std::fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_the_first_line_trimmed() {
        assert_eq!(
            read_line(&b"  wok_abc123  \nmore\n"[..]).unwrap(),
            "wok_abc123"
        );
        assert_eq!(read_line(&b""[..]).unwrap(), "");
    }

    #[test]
    fn a_blank_or_spaced_token_is_refused_and_nothing_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(None, dir.path().join("wokwi-token"));
        assert_eq!(save(&store, "  ").unwrap_err().code, "no_token");
        assert_eq!(save(&store, "two words").unwrap_err().code, "no_token");
        assert!(store.load().unwrap().is_none());
        assert_eq!(save(&store, "wok_abc").unwrap(), "file");
        assert_eq!(store.load().unwrap().unwrap().0, "wok_abc");
    }

    #[test]
    fn a_python_is_new_enough_from_3_9() {
        assert_eq!(python_version("Python 3.12.3\n"), Some((3, 12)));
        assert_eq!(python_version("Python 2.7.18"), Some((2, 7)));
        assert_eq!(python_version("zsh: command not found"), None);
        assert!(python_version("Python 3.9.0").unwrap() >= MIN_PYTHON);
        assert!(python_version("Python 3.8.19").unwrap() < MIN_PYTHON);
    }

    #[test]
    fn the_package_must_be_in_the_range_pip_is_asked_for() {
        assert!(package_fits(sim86lib::TESTED));
        assert!(package_fits("0.1.9"));
        assert!(!package_fits("0.1.3"));
        assert!(!package_fits("0.2.0"));
        assert!(!package_fits(""));
        assert!(sim86lib::PACKAGE.contains(sim86lib::TESTED));
    }

    #[test]
    fn this_machine_has_a_release() {
        assert!(asset().unwrap().starts_with("wokwi-cli-"));
    }
}
