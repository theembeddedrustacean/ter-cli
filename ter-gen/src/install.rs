//! What `ter install` has to do on this machine: which tools are already
//! there, and the commands that install the ones that are not.
//!
//! Planning is separate from doing. [`plan`] only looks (it runs
//! `rustup ... list` and `<tool> --version`, nothing that changes the
//! machine) and returns one [`Item`] per thing a tool needs. The caller runs
//! the [`Cmd`]s of the items that are missing. Running `ter install` twice
//! is therefore safe: the second plan finds everything present and runs
//! nothing.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The tools `ter install` knows how to install. `wokwi-cli` is downloaded
/// by the `ter` binary itself, with the learner's Wokwi token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// The Rust target (and toolchain, components, linker) of a project.
    Targets,
    Espflash,
    ProbeRs,
}

/// A cargo-installed tool: its program, its crate and the oldest major
/// version ter works with.
struct CargoTool {
    program: &'static str,
    krate: &'static str,
    min_major: u64,
}

// ter flashes with `espflash flash --non-interactive --after no-reset`,
// which espflash 4 introduced.
const ESPFLASH: CargoTool = CargoTool {
    program: "espflash",
    krate: "espflash",
    min_major: 4,
};

const PROBE_RS: CargoTool = CargoTool {
    program: "probe-rs",
    krate: "probe-rs-tools",
    min_major: 0,
};

const LDPROXY: CargoTool = CargoTool {
    program: "ldproxy",
    krate: "ldproxy",
    min_major: 0,
};

const ESPUP: CargoTool = CargoTool {
    program: "espup",
    krate: "espup",
    min_major: 0,
};

/// A command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.program)?;
        for a in &self.args {
            write!(f, " {a}")?;
        }
        Ok(())
    }
}

/// One thing a tool needs, and whether this machine has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What it is, for people: `espflash`, `Rust target
    /// riscv32imc-unknown-none-elf (stable)`.
    pub what: String,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Already there; what was found (a path, a version).
    Present(String),
    /// Missing (or too old, said in `why`); these commands install it.
    Install { why: String, run: Vec<Cmd> },
    /// Missing, and ter cannot install it: what the learner does instead.
    Manual(String),
}

/// What ter asks of the machine while planning. The real one runs
/// programs; tests use a fake.
pub trait Probe {
    /// Where `program` is: the PATH, then cargo's bin folder.
    fn find(&self, program: &str) -> Option<PathBuf>;
    /// The program's standard output, if it ran and succeeded.
    fn output(&self, program: &Path, args: &[&str]) -> Option<String>;
}

/// This machine.
pub struct System;

impl Probe for System {
    fn find(&self, program: &str) -> Option<PathBuf> {
        find_program(program)
    }

    fn output(&self, program: &Path, args: &[&str]) -> Option<String> {
        let out = Command::new(program).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// `program` on the PATH, else in cargo's bin folder (`$CARGO_HOME/bin`,
/// `~/.cargo/bin`), where `cargo install` puts it even when that folder is
/// not on the PATH yet.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(cargo_bin())
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

fn cargo_bin() -> Option<PathBuf> {
    match std::env::var_os("CARGO_HOME") {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home).join("bin")),
        _ => std::env::home_dir().map(|h| h.join(".cargo").join("bin")),
    }
}

/// What a project builds for, read from its `.cargo/config.toml` and
/// `rust-toolchain.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectTarget {
    pub triple: String,
    /// The toolchain `rust-toolchain.toml` pins, if any.
    pub channel: Option<String>,
    /// `[unstable] build-std`: cargo builds the standard library from
    /// source, so rustup needs `rust-src`, not the target.
    pub build_std: bool,
    /// `components` from `rust-toolchain.toml`.
    pub components: Vec<String>,
    /// The project links through `ldproxy` (the ESP-IDF std stack).
    pub ldproxy: bool,
}

/// Read `dir`'s target. The error says what is missing, for people.
pub fn read_project(dir: &Path) -> Result<ProjectTarget, String> {
    let config_path = dir.join(".cargo").join("config.toml");
    let text = std::fs::read_to_string(&config_path).map_err(|_| {
        format!(
            "{} is not a project ter can read a target from: it has no .cargo/config.toml.",
            dir.display()
        )
    })?;
    let config: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("{} is not valid TOML: {e}", config_path.display()))?;
    let triple = config
        .get("build")
        .and_then(|b| b.get("target"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("{} names no [build] target.", config_path.display()))?
        .to_string();
    let build_std = config
        .get("unstable")
        .and_then(|u| u.get("build-std"))
        .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
    let ldproxy = config
        .get("target")
        .and_then(|t| t.get(&triple))
        .and_then(|t| t.get("linker"))
        .and_then(toml::Value::as_str)
        == Some("ldproxy");

    let mut project = ProjectTarget {
        triple,
        build_std,
        ldproxy,
        ..Default::default()
    };
    let pinned = dir.join("rust-toolchain.toml");
    if let Ok(text) = std::fs::read_to_string(&pinned) {
        let file: toml::Table = toml::from_str(&text)
            .map_err(|e| format!("{} is not valid TOML: {e}", pinned.display()))?;
        if let Some(t) = file.get("toolchain") {
            project.channel = t
                .get("channel")
                .and_then(toml::Value::as_str)
                .map(str::to_string);
            project.components = t
                .get("components")
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
        }
    }
    Ok(project)
}

/// What installing `tool` takes on this machine. `project` is needed for
/// [`Tool::Targets`] only.
pub fn plan(probe: &impl Probe, tool: Tool, project: Option<&ProjectTarget>) -> Vec<Item> {
    match tool {
        Tool::Espflash => vec![cargo_tool(probe, &ESPFLASH)],
        Tool::ProbeRs => vec![cargo_tool(probe, &PROBE_RS)],
        Tool::Targets => project.map(|p| targets(probe, p)).unwrap_or_default(),
    }
}

fn cargo_tool(probe: &impl Probe, tool: &CargoTool) -> Item {
    let install = |why: String, force: bool| {
        let mut args = vec!["install", tool.krate, "--locked"];
        if force {
            args.push("--force");
        }
        State::Install {
            why,
            run: vec![Cmd::new("cargo", &args)],
        }
    };
    let state = match probe.find(tool.program) {
        None => install("not installed".into(), false),
        Some(path) => {
            let version = probe
                .output(&path, &["--version"])
                .and_then(|out| version(&out));
            match version {
                Some((v, major)) if major < tool.min_major => install(
                    format!(
                        "{} {v} is too old: ter needs {} or newer",
                        tool.program, tool.min_major
                    ),
                    true,
                ),
                Some((v, _)) => State::Present(format!("{v}, {}", path.display())),
                None => State::Present(path.display().to_string()),
            }
        }
    };
    Item {
        what: tool.program.into(),
        state,
    }
}

/// The version a `--version` line names (`espflash 4.6.0`), and its major.
fn version(out: &str) -> Option<(String, u64)> {
    let word = out
        .split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()) && w.contains('.'))?;
    let major = word.split('.').next()?.parse().ok()?;
    Some((word.to_string(), major))
}

fn targets(probe: &impl Probe, project: &ProjectTarget) -> Vec<Item> {
    let Some(rustup) = probe.find("rustup") else {
        return vec![Item {
            what: "rustup".into(),
            state: State::Manual(
                "Rust is installed with rustup, which is not on this machine. Install it from https://rustup.rs, then run this again."
                    .into(),
            ),
        }];
    };
    let mut items = Vec::new();

    // Xtensa (ESP32, ESP32-S2, ESP32-S3) is not a target rustup ships:
    // Espressif's toolchain, `esp`, is installed by espup.
    if project.triple.starts_with("xtensa-") {
        let toolchains = probe
            .output(&rustup, &["toolchain", "list"])
            .unwrap_or_default();
        let state = if has_toolchain(&toolchains, "esp") {
            State::Present("the esp toolchain".into())
        } else {
            let mut run = Vec::new();
            if probe.find(ESPUP.program).is_none() {
                run.push(Cmd::new("cargo", &["install", ESPUP.krate, "--locked"]));
            }
            run.push(Cmd::new("espup", &["install"]));
            State::Install {
                why: "not installed".into(),
                run,
            }
        };
        items.push(Item {
            what: format!("Espressif Rust toolchain for {}", project.triple),
            state,
        });
        if project.ldproxy {
            items.push(cargo_tool(probe, &LDPROXY));
        }
        return items;
    }

    let channel = project.channel.as_deref();
    let on = |what: String| match channel {
        Some(c) => format!("{what} ({c})"),
        None => what,
    };
    let with_toolchain = |args: &[&str]| {
        let mut v: Vec<&str> = args.to_vec();
        if let Some(c) = channel {
            v.extend(["--toolchain", c]);
        }
        v.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    };
    let rustup_cmd = |args: Vec<String>| Cmd {
        program: "rustup".into(),
        args,
    };

    // A pinned toolchain that is not installed yet: install it first, and
    // everything on it is missing too.
    let mut toolchain_missing = false;
    if let Some(c) = channel {
        let toolchains = probe
            .output(&rustup, &["toolchain", "list"])
            .unwrap_or_default();
        let state = if has_toolchain(&toolchains, c) {
            State::Present(c.to_string())
        } else {
            toolchain_missing = true;
            State::Install {
                why: "not installed".into(),
                run: vec![Cmd::new(
                    "rustup",
                    &["toolchain", "install", c, "--profile", "minimal"],
                )],
            }
        };
        items.push(Item {
            what: format!("Rust toolchain {c}"),
            state,
        });
    }

    let mut components = project.components.clone();
    if project.build_std && !components.iter().any(|c| c == "rust-src") {
        components.push("rust-src".into());
    }
    let installed_components = if toolchain_missing {
        String::new()
    } else {
        probe
            .output(
                &rustup,
                &with_toolchain(&["component", "list", "--installed"])
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default()
    };
    for component in &components {
        let present = installed_components
            .lines()
            .map(str::trim)
            .any(|l| l == component || l.starts_with(&format!("{component}-")));
        let state = if present {
            State::Present(component.clone())
        } else {
            State::Install {
                why: "not installed".into(),
                run: vec![rustup_cmd(with_toolchain(&["component", "add", component]))],
            }
        };
        items.push(Item {
            what: on(format!("Rust component {component}")),
            state,
        });
    }

    // With build-std cargo compiles core itself: there is no target to add
    // (the ESP-IDF std triples are not even one rustup ships).
    if !project.build_std {
        let installed = if toolchain_missing {
            String::new()
        } else {
            probe
                .output(
                    &rustup,
                    &with_toolchain(&["target", "list", "--installed"])
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_default()
        };
        let state = if installed.lines().any(|l| l.trim() == project.triple) {
            State::Present(project.triple.clone())
        } else {
            State::Install {
                why: "not installed".into(),
                run: vec![rustup_cmd(with_toolchain(&[
                    "target",
                    "add",
                    &project.triple,
                ]))],
            }
        };
        items.push(Item {
            what: on(format!("Rust target {}", project.triple)),
            state,
        });
    }

    if project.ldproxy {
        items.push(cargo_tool(probe, &LDPROXY));
    }
    items
}

/// `rustup toolchain list` names toolchains with the host appended
/// (`stable-x86_64-unknown-linux-gnu (default)`); `esp` stands alone.
fn has_toolchain(list: &str, channel: &str) -> bool {
    list.lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|name| name == channel || name.starts_with(&format!("{channel}-")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Programs that exist, and what each `program args` prints.
    #[derive(Default)]
    struct Fake {
        programs: Vec<&'static str>,
        outputs: HashMap<String, String>,
    }

    impl Fake {
        fn with(mut self, program: &'static str) -> Self {
            self.programs.push(program);
            self
        }
        fn says(mut self, command: &str, out: &str) -> Self {
            self.outputs.insert(command.into(), out.into());
            self
        }
    }

    impl Probe for Fake {
        fn find(&self, program: &str) -> Option<PathBuf> {
            self.programs
                .contains(&program)
                .then(|| PathBuf::from(program))
        }
        fn output(&self, program: &Path, args: &[&str]) -> Option<String> {
            let key = format!("{} {}", program.display(), args.join(" "));
            self.outputs.get(&key).cloned()
        }
    }

    fn commands(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .flat_map(|i| match &i.state {
                State::Install { run, .. } => run.iter().map(|c| c.to_string()).collect(),
                _ => vec![],
            })
            .collect()
    }

    fn c3() -> ProjectTarget {
        ProjectTarget {
            triple: "riscv32imc-unknown-none-elf".into(),
            channel: Some("stable".into()),
            components: vec!["rust-src".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_missing_cargo_tool_is_cargo_installed_and_a_present_one_is_left_alone() {
        let none = Fake::default();
        assert_eq!(
            commands(&plan(&none, Tool::Espflash, None)),
            ["cargo install espflash --locked"]
        );
        assert_eq!(
            commands(&plan(&none, Tool::ProbeRs, None)),
            ["cargo install probe-rs-tools --locked"]
        );

        let there = Fake::default()
            .with("espflash")
            .says("espflash --version", "espflash 4.6.0\n");
        let items = plan(&there, Tool::Espflash, None);
        assert_eq!(items[0].state, State::Present("4.6.0, espflash".into()));
    }

    #[test]
    fn an_espflash_older_than_4_is_replaced() {
        let old = Fake::default()
            .with("espflash")
            .says("espflash --version", "espflash 3.3.0\n");
        let items = plan(&old, Tool::Espflash, None);
        let State::Install { why, run } = &items[0].state else {
            panic!("{items:?}")
        };
        assert_eq!(why, "espflash 3.3.0 is too old: ter needs 4 or newer");
        assert_eq!(
            run[0].to_string(),
            "cargo install espflash --locked --force"
        );
    }

    #[test]
    fn versions_are_read_off_the_version_line() {
        assert_eq!(version("espflash 4.6.0"), Some(("4.6.0".into(), 4)));
        assert_eq!(
            version("probe-rs 0.32.0 (git commit: abc)"),
            Some(("0.32.0".into(), 0))
        );
        assert_eq!(version("wokwi-cli v0.27.1"), Some(("0.27.1".into(), 0)));
        assert_eq!(version("nothing here"), None);
    }

    #[test]
    fn a_target_on_a_pinned_toolchain_is_added_to_that_toolchain() {
        let host = Fake::default()
            .with("rustup")
            .says(
                "rustup toolchain list",
                "stable-x86_64-unknown-linux-gnu (default)\n",
            )
            .says(
                "rustup component list --installed --toolchain stable",
                "cargo-x86_64-unknown-linux-gnu\nrustc-x86_64-unknown-linux-gnu\n",
            )
            .says(
                "rustup target list --installed --toolchain stable",
                "x86_64-unknown-linux-gnu\n",
            );
        assert_eq!(
            commands(&plan(&host, Tool::Targets, Some(&c3()))),
            [
                "rustup component add rust-src --toolchain stable",
                "rustup target add riscv32imc-unknown-none-elf --toolchain stable",
            ]
        );
    }

    #[test]
    fn everything_present_plans_nothing() {
        let host = Fake::default()
            .with("rustup")
            .says(
                "rustup toolchain list",
                "stable-x86_64-unknown-linux-gnu (default)\n",
            )
            .says(
                "rustup component list --installed --toolchain stable",
                "rust-src\n",
            )
            .says(
                "rustup target list --installed --toolchain stable",
                "riscv32imc-unknown-none-elf\nx86_64-unknown-linux-gnu\n",
            );
        let items = plan(&host, Tool::Targets, Some(&c3()));
        assert!(commands(&items).is_empty(), "{items:?}");
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn a_missing_pinned_toolchain_is_installed_before_its_target() {
        let host = Fake::default()
            .with("rustup")
            .says("rustup toolchain list", "1.98.1-x86_64-unknown-linux-gnu\n");
        let project = ProjectTarget {
            triple: "thumbv7em-none-eabihf".into(),
            channel: Some("stable".into()),
            ..Default::default()
        };
        assert_eq!(
            commands(&plan(&host, Tool::Targets, Some(&project))),
            [
                "rustup toolchain install stable --profile minimal",
                "rustup target add thumbv7em-none-eabihf --toolchain stable",
            ]
        );
    }

    #[test]
    fn build_std_needs_rust_src_and_no_target_and_ldproxy_is_a_cargo_tool() {
        let host = Fake::default().with("rustup").says(
            "rustup toolchain list",
            "nightly-x86_64-unknown-linux-gnu\n",
        );
        let std_c3 = ProjectTarget {
            triple: "riscv32imc-esp-espidf".into(),
            channel: Some("nightly".into()),
            build_std: true,
            ldproxy: true,
            ..Default::default()
        };
        assert_eq!(
            commands(&plan(&host, Tool::Targets, Some(&std_c3))),
            [
                "rustup component add rust-src --toolchain nightly",
                "cargo install ldproxy --locked",
            ]
        );
    }

    #[test]
    fn no_pinned_toolchain_uses_the_default_one() {
        let host = Fake::default().with("rustup").says(
            "rustup target list --installed",
            "x86_64-unknown-linux-gnu\n",
        );
        let project = ProjectTarget {
            triple: "thumbv6m-none-eabi".into(),
            ..Default::default()
        };
        assert_eq!(
            commands(&plan(&host, Tool::Targets, Some(&project))),
            ["rustup target add thumbv6m-none-eabi"]
        );
    }

    #[test]
    fn xtensa_comes_from_espup() {
        let project = ProjectTarget {
            triple: "xtensa-esp32s3-none-elf".into(),
            channel: Some("esp".into()),
            ..Default::default()
        };
        let bare = Fake::default()
            .with("rustup")
            .says("rustup toolchain list", "stable-x86_64-unknown-linux-gnu\n");
        assert_eq!(
            commands(&plan(&bare, Tool::Targets, Some(&project))),
            ["cargo install espup --locked", "espup install"]
        );
        let done = Fake::default().with("rustup").with("espup").says(
            "rustup toolchain list",
            "esp\nstable-x86_64-unknown-linux-gnu\n",
        );
        assert!(commands(&plan(&done, Tool::Targets, Some(&project))).is_empty());
    }

    #[test]
    fn no_rustup_is_a_manual_step() {
        let items = plan(&Fake::default(), Tool::Targets, Some(&c3()));
        assert!(matches!(items[0].state, State::Manual(ref m) if m.contains("https://rustup.rs")));
    }

    #[test]
    fn toolchain_names_match_with_the_host_appended() {
        let list = "stable-x86_64-unknown-linux-gnu (default)\n1.98.1-x86_64-unknown-linux-gnu (active)\nesp\n";
        assert!(has_toolchain(list, "stable"));
        assert!(has_toolchain(list, "1.98.1"));
        assert!(has_toolchain(list, "esp"));
        assert!(!has_toolchain(list, "1.98"));
        assert!(!has_toolchain(list, "nightly"));
    }

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    #[test]
    fn a_project_target_is_read_from_its_cargo_config_and_toolchain_file() {
        let dir = project(&[
            (
                ".cargo/config.toml",
                "[target.riscv32imc-unknown-none-elf]\nrunner = \"espflash flash --monitor --chip esp32c3\"\n\n[build]\ntarget = \"riscv32imc-unknown-none-elf\"\n",
            ),
            (
                "rust-toolchain.toml",
                "[toolchain]\nchannel = \"stable\"\ncomponents = [\"rust-src\"]\ntargets = [\"riscv32imc-unknown-none-elf\"]\n",
            ),
        ]);
        assert_eq!(read_project(dir.path()).unwrap(), c3());
    }

    #[test]
    fn a_std_project_is_build_std_and_links_through_ldproxy() {
        let dir = project(&[(
            ".cargo/config.toml",
            "[build]\ntarget = \"riscv32imc-esp-espidf\"\n\n[target.riscv32imc-esp-espidf]\nlinker = \"ldproxy\"\n\n[unstable]\nbuild-std = [\"std\", \"panic_abort\"]\n",
        )]);
        let p = read_project(dir.path()).unwrap();
        assert!(p.build_std && p.ldproxy);
        assert_eq!(p.channel, None);
    }

    #[test]
    fn a_folder_without_a_target_says_what_is_missing() {
        let empty = project(&[]);
        assert!(
            read_project(empty.path())
                .unwrap_err()
                .contains("has no .cargo/config.toml")
        );
        let untargeted = project(&[(".cargo/config.toml", "[env]\nX = \"1\"\n")]);
        assert!(
            read_project(untargeted.path())
                .unwrap_err()
                .contains("names no [build] target")
        );
    }
}
