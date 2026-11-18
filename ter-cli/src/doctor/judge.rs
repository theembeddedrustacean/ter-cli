//! From what was found on the machine to the report. Nothing here looks at
//! the machine: [`judge`] is the same for a real one and for a test's.

use std::path::PathBuf;

use serde::Serialize;
use ter_flash::{PortKind, Tool};
use ter_gen::install::{Item, NOT_INSTALLED, State, at_least, same_version};
use ter_sdk::version::{check_supported, newer_available};

use crate::hw::{PORT_ENV, graded_only};
use crate::sim::TOKEN_PAGE;
use crate::survey::Survey;

/// Where the probe-rs udev rules and how to install them are.
pub const PROBE_SETUP_PAGE: &str = "https://probe.rs/docs/getting-started/probe-setup/";
const RUSTUP_PAGE: &str = "https://rustup.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    /// Not relevant in this folder, or not looked at (`--offline`).
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub group: &'static str,
    pub status: Status,
    /// What is there.
    pub found: String,
    pub why: Option<String>,
    pub fix: Option<String>,
}

impl Check {
    fn new(id: &'static str, status: Status, found: impl Into<String>) -> Self {
        Self {
            id,
            group: match id.split('.').next().unwrap_or(id) {
                "tool" => "tools",
                "courses" => "disk",
                group => group,
            },
            status,
            found: found.into(),
            why: None,
            fix: None,
        }
    }
    fn ok(id: &'static str, found: impl Into<String>) -> Self {
        Self::new(id, Status::Ok, found)
    }
    fn warn(id: &'static str, found: impl Into<String>) -> Self {
        Self::new(id, Status::Warn, found)
    }
    fn fail(id: &'static str, found: impl Into<String>) -> Self {
        Self::new(id, Status::Fail, found)
    }
    fn skip(id: &'static str, found: impl Into<String>) -> Self {
        Self::new(id, Status::Skip, found)
    }
    fn why(mut self, why: impl Into<String>) -> Self {
        self.why = Some(why.into());
        self
    }
    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Context {
    pub dir: PathBuf,
    /// `exercise`, `project` or `none`.
    pub kind: &'static str,
    pub mode: Option<String>,
    pub venue: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Nothing failed; warnings do not count.
    pub ok: bool,
    pub context: Context,
    pub checks: Vec<Check>,
    /// What `ter install` can put right, in the order it installs.
    pub install: Vec<&'static str>,
}

impl Report {
    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }
}

/// What the site said to the one `ping`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Site {
    /// `--offline`: not asked.
    Offline,
    /// No token on this machine, so nothing to ask with.
    NoToken,
    Answer {
        user: String,
        min_supported: String,
        latest: String,
        premium: Option<bool>,
    },
    /// The error's code and message, as ter would print them.
    Failed { code: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    None,
    /// Has `.cargo/config.toml` and no `ter.toml`: made by `ter new`.
    Project,
    Exercise,
}

/// The folder doctor was run in, read without building anything.
#[derive(Debug, Clone, Default)]
pub struct Folder {
    pub dir: PathBuf,
    pub kind: Kind,
    pub mode: Option<String>,
    pub venue: Option<String>,
    /// Why the exercise cannot be run as it is: its ter.toml or
    /// check.yaml does not read, or names a mode or venue ter cannot use.
    pub broken: Option<String>,
    /// The flashing tool its runner names, or why ter cannot use it.
    pub tool: Option<Result<Tool, String>>,
    /// The program the runner starts, which `cargo run` needs.
    pub runner_program: Option<String>,
    /// Whether Wokwi can run the checks, for a simulation exercise.
    pub sim_plan: Option<Result<(), String>>,
    pub sim_allowed: bool,
    /// How many checks check.yaml has; `None` without one.
    pub checks: Option<usize>,
    /// The checks a bare board cannot grade.
    pub unseen: Vec<String>,
}

/// One cargo-installed tool, as `ter install` sees it.
#[derive(Debug, Clone)]
pub struct ToolFact {
    pub name: &'static str,
    pub state: State,
    pub version: Option<String>,
}

/// The versions a curriculum checkout is checked with (`--dev`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub xiao_generate: Option<String>,
    pub espflash: Option<String>,
    pub probe_rs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Targets {
    NoProject,
    Unreadable(String),
    Items(Vec<Item>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Root {
    Exists,
    WillCreate,
    /// Why nothing can be written there.
    Blocked(String),
}

/// Whether this login may read and write a device node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAccess {
    /// What it is, for people: `/dev/ttyACM0`, `debug probe 0483:374b`.
    pub what: String,
    pub node: String,
    pub ok: bool,
    /// The group that owns the node, when it has a name here.
    pub group: Option<String>,
    /// The account is in that group, though this login is not.
    pub listed: bool,
}

/// Everything doctor found.
#[derive(Debug, Clone)]
pub struct Facts {
    pub cli_version: String,
    pub site_url: String,
    /// `TER_TOKEN`, `keychain`, `file` or `none`.
    pub token_source: &'static str,
    pub site: Site,
    pub rustup: Option<String>,
    pub cargo: Option<String>,
    /// `rustup toolchain list`, one toolchain a line.
    pub toolchains: Vec<String>,
    pub targets: Targets,
    pub tools: Vec<ToolFact>,
    pub survey: Survey,
    /// `TER_PORT` names a port that is there.
    pub port_env_present: bool,
    pub courses_root: PathBuf,
    pub root: Root,
    pub linux: bool,
    pub serial: Vec<NodeAccess>,
    pub probes: Vec<NodeAccess>,
    pub folder: Folder,
    pub pins: Option<Pins>,
}

/// What the folder's run needs from this machine.
struct Needs<'a> {
    /// The tool the board is flashed with.
    board: Option<&'a Tool>,
    /// The run cannot happen without the board.
    board_required: bool,
    /// The program that has to be installed for it.
    program: Option<&'a str>,
    wokwi: bool,
}

fn needs(folder: &Folder) -> Needs<'_> {
    let tool = folder.tool.as_ref().and_then(|t| t.as_ref().ok());
    let none = Needs {
        board: None,
        board_required: false,
        program: None,
        wokwi: false,
    };
    if folder.broken.is_some() {
        return none;
    }
    match (folder.kind, folder.mode.as_deref(), folder.venue.as_deref()) {
        (Kind::Exercise, Some("hardware"), Some("local")) => Needs {
            board: tool,
            board_required: true,
            program: tool.filter(|t| t.external()).map(Tool::program),
            ..none
        },
        (Kind::Exercise, Some("simulation"), _) => Needs {
            wokwi: true,
            ..none
        },
        // `cargo run` starts the runner's own program.
        (Kind::Project, ..) => Needs {
            board: tool,
            program: folder.runner_program.as_deref(),
            ..none
        },
        _ => none,
    }
}

pub fn judge(facts: &Facts) -> Report {
    let needs = needs(&facts.folder);
    let mut checks = Vec::new();
    let mut install: Vec<&'static str> = Vec::new();

    checks.extend(site(facts));
    checks.extend(rust(facts));

    let targets = rust_targets(&facts.targets);
    if targets.status == Status::Fail && matches!(facts.targets, Targets::Items(_)) {
        install.push("targets");
    }
    checks.push(targets);

    for tool in &facts.tools {
        let check = cargo_tool(tool, needs.program == Some(tool.name), facts.pins.as_ref());
        if check.status == Status::Fail {
            install.push(tool.name);
        }
        checks.push(check);
    }

    let wokwi = wokwi(&facts.survey, needs.wokwi);
    if wokwi.iter().any(|c| c.status == Status::Fail) {
        install.push("wokwi-cli");
    }
    checks.extend(wokwi);

    checks.push(courses_root(facts));
    checks.push(boards(facts, &needs));
    checks.push(probes(&facts.survey));
    checks.push(serial(facts, &needs));
    checks.push(probe_usb(facts, &needs));
    let venue = venue(&facts.folder, &checks);
    checks.push(venue);

    install.sort_by_key(|name| {
        crate::install::TOOLS
            .iter()
            .position(|t| t == name)
            .unwrap_or(usize::MAX)
    });
    let folder = &facts.folder;
    Report {
        ok: checks.iter().all(|c| c.status != Status::Fail),
        context: Context {
            dir: folder.dir.clone(),
            kind: match folder.kind {
                Kind::None => "none",
                Kind::Project => "project",
                Kind::Exercise => "exercise",
            },
            mode: folder.mode.clone(),
            venue: folder.venue.clone(),
        },
        checks,
        install,
    }
}

/// `ter.version`, `site.reach`, `account.token`, `account.premium`: all
/// from the one `ping`.
fn site(facts: &Facts) -> [Check; 4] {
    let source = facts.token_source;
    let no_token = || {
        Check::fail("account.token", "no device token on this machine")
            .fix("ter login (or set TER_TOKEN)")
    };
    let unasked = |why: &str| {
        [
            Check::skip("ter.version", facts.cli_version.as_str()).why(why),
            Check::skip("site.reach", facts.site_url.as_str()).why(why),
            Check::skip("account.premium", "not known").why(why),
        ]
    };
    let [version, reach, token, premium] = match &facts.site {
        Site::Offline => {
            let [version, reach, premium] = unasked("not asked: --offline");
            let token = if source == "none" {
                no_token()
            } else {
                Check::ok("account.token", format!("from {source}"))
                    .why("not checked with the site: --offline")
            };
            [version, reach, token, premium]
        }
        Site::NoToken => {
            let [version, reach, premium] = unasked("not asked: there is no token to ask with");
            [version, reach, no_token(), premium]
        }
        Site::Answer {
            user,
            min_supported,
            latest,
            premium,
        } => {
            let current = facts.cli_version.as_str();
            let version = if check_supported(current, min_supported).is_err() {
                Check::fail("ter.version", current)
                    .why(format!(
                        "the site accepts {} and later",
                        min_supported.trim()
                    ))
                    .fix("ter self-update")
            } else if let Some(newer) = newer_available(current, latest) {
                Check::warn("ter.version", current)
                    .why(format!("ter {newer} is available"))
                    .fix("ter self-update")
            } else {
                Check::ok("ter.version", current)
            };
            let premium = match premium {
                Some(true) => Check::ok("account.premium", "yes"),
                Some(false) => Check::warn("account.premium", "no")
                    .why("exercises need TER Premium")
                    .fix(format!("See {}", facts.site_url)),
                None => Check::skip("account.premium", "not known").why("the site did not say"),
            };
            [
                version,
                Check::ok("site.reach", facts.site_url.as_str()),
                Check::ok("account.token", format!("{user}, from {source}")),
                premium,
            ]
        }
        Site::Failed { code, message } => {
            let token_refused = code == "token_invalid";
            let reach = match code.as_str() {
                "token_invalid" => Check::ok("site.reach", facts.site_url.as_str()),
                "rate_limited" => Check::warn("site.reach", facts.site_url.as_str())
                    .why(message.as_str())
                    .fix("Wait a minute, then run ter doctor again."),
                "network_error" => Check::fail("site.reach", facts.site_url.as_str())
                    .why(message.as_str())
                    .fix(format!(
                        "Check your connection, and that {} opens in a browser.",
                        facts.site_url
                    )),
                _ => Check::fail("site.reach", facts.site_url.as_str())
                    .why(message.as_str())
                    .fix("The site answered with an error: try again later."),
            };
            let token = if token_refused {
                // Reported only: the token stays where it is.
                Check::fail(
                    "account.token",
                    format!("from {source}, refused by the site"),
                )
                .why(message.as_str())
                .fix(if source == "TER_TOKEN" {
                    "Unset TER_TOKEN or set it to a valid token, or pair with ter login"
                } else {
                    "ter login"
                })
            } else {
                Check::skip("account.token", format!("from {source}"))
                    .why("not checked: the site gave no answer")
            };
            let [version, _, premium] = unasked("not known: the site gave no answer");
            [version, reach, token, premium]
        }
    };
    [version, reach, token, premium]
}

/// `rust.rustup`, `rust.cargo`, `rust.toolchain`.
fn rust(facts: &Facts) -> [Check; 3] {
    let install = format!("Install Rust from {RUSTUP_PAGE}");
    let program = |id: &'static str, found: &Option<String>| match found {
        Some(path) => Check::ok(id, path.as_str()),
        None => Check::fail(id, "not installed").fix(install.as_str()),
    };
    let toolchain = if facts.rustup.is_none() {
        Check::skip("rust.toolchain", "not known").why("rustup is not installed")
    } else {
        let default = facts
            .toolchains
            .iter()
            .find(|t| t.contains("(default)"))
            .or(facts.toolchains.first());
        match default {
            Some(t) => Check::ok(
                "rust.toolchain",
                t.split_whitespace().next().unwrap_or(t.as_str()),
            ),
            None => Check::fail("rust.toolchain", "no Rust toolchain installed")
                .fix("rustup default stable"),
        }
    };
    [
        program("rust.rustup", &facts.rustup),
        program("rust.cargo", &facts.cargo),
        toolchain,
    ]
}

fn rust_targets(targets: &Targets) -> Check {
    const ID: &str = "rust.targets";
    match targets {
        Targets::NoProject => {
            Check::skip(ID, "not looked at").why("not in an exercise or project folder")
        }
        Targets::Unreadable(why) => {
            Check::fail(ID, "the folder's target cannot be read").why(why.as_str())
        }
        Targets::Items(items) => {
            let missing: Vec<&str> = items
                .iter()
                .filter(|i| !matches!(i.state, State::Present(_)))
                .map(|i| i.what.as_str())
                .collect();
            if missing.is_empty() {
                let all: Vec<&str> = items.iter().map(|i| i.what.as_str()).collect();
                return Check::ok(ID, all.join(", "));
            }
            let manual = items.iter().find_map(|i| match &i.state {
                State::Manual(what_to_do) => Some(what_to_do.as_str()),
                _ => None,
            });
            Check::fail(ID, format!("missing: {}", missing.join(", ")))
                .fix(manual.unwrap_or("ter install targets"))
        }
    }
}

fn tool_id(name: &str) -> &'static str {
    match name {
        "espflash" => "tool.espflash",
        "probe-rs" => "tool.probe-rs",
        "elf2uf2-rs" => "tool.elf2uf2-rs",
        "uf2deploy" => "tool.uf2deploy",
        _ => "tool.xiao-generate",
    }
}

fn cargo_tool(tool: &ToolFact, needed: bool, pins: Option<&Pins>) -> Check {
    let id = tool_id(tool.name);
    let fix = format!("ter install {}", tool.name);
    match &tool.state {
        State::Present(found) => {
            let ok = Check::ok(id, found.as_str());
            let (Some(pins), Some(version)) = (pins, &tool.version) else {
                return ok;
            };
            // The generator's output changes with its version, so the
            // curriculum pins it exactly; the flashing tools have a floor.
            let (pin, exact, krate) = match tool.name {
                "xiao-generate" => (&pins.xiao_generate, true, "xiao-generate"),
                "espflash" => (&pins.espflash, false, "espflash"),
                "probe-rs" => (&pins.probe_rs, false, "probe-rs-tools"),
                _ => return ok,
            };
            let Some(pin) = pin else { return ok };
            let matches = if exact {
                same_version(version, pin)
            } else {
                at_least(version, pin)
            };
            if matches {
                return ok;
            }
            let mut check = Check::warn(id, found.as_str()).why(format!(
                "the curriculum is checked with {} {pin}{}",
                tool.name,
                if exact { "" } else { " or newer" }
            ));
            check.fix = Some(if exact {
                format!("cargo install {krate} --locked --force --version {pin}")
            } else {
                format!("cargo install {krate} --locked --force")
            });
            check
        }
        State::Install { why, .. } if why == NOT_INSTALLED => {
            if needed {
                Check::fail(id, NOT_INSTALLED).fix(fix)
            } else {
                Check::skip(id, NOT_INSTALLED).why(format!("not needed here; `{fix}` installs it"))
            }
        }
        // Too old. A generator below the floor writes projects that do
        // not match the exercises, wherever doctor is run.
        State::Install { why, .. } => {
            if needed || tool.name == "xiao-generate" {
                Check::fail(id, why.as_str()).fix(fix)
            } else {
                Check::warn(id, why.as_str()).fix(fix)
            }
        }
        State::Manual(what_to_do) => Check::fail(id, what_to_do.as_str()),
    }
}

/// `tool.wokwi-cli` and `wokwi.token`. Wokwi itself is never asked.
fn wokwi(survey: &Survey, needed: bool) -> [Check; 2] {
    let not_needed = "not needed here; `ter install wokwi-cli` sets it up";
    let cli = match (&survey.wokwi_cli, needed) {
        (Some(path), _) => Check::ok("tool.wokwi-cli", path.display().to_string()),
        (None, true) => Check::fail("tool.wokwi-cli", NOT_INSTALLED).fix("ter install wokwi-cli"),
        (None, false) => Check::skip("tool.wokwi-cli", NOT_INSTALLED).why(not_needed),
    };
    let token = match (survey.wokwi_token, needed) {
        (Some(t), _) if t.well_formed => Check::ok("wokwi.token", format!("from {}", t.source)),
        (Some(t), _) => Check::warn("wokwi.token", format!("from {}", t.source))
            .why("it does not look like a Wokwi CI token (wok_ and 40 letters and digits)")
            .fix(format!(
                "Create a CI token at {TOKEN_PAGE}, then: ter install wokwi-cli --token-stdin"
            )),
        (None, true) => Check::fail("wokwi.token", "none")
            .why(format!(
                "simulation runs on your own Wokwi account: create a CI token at {TOKEN_PAGE}"
            ))
            .fix("ter install wokwi-cli"),
        (None, false) => Check::skip("wokwi.token", "none").why(not_needed),
    };
    [cli, token]
}

fn courses_root(facts: &Facts) -> Check {
    const ID: &str = "courses.root";
    let path = facts.courses_root.display().to_string();
    match &facts.root {
        Root::Exists => Check::ok(ID, path),
        Root::WillCreate => Check::ok(ID, path).why("made by the first `ter ex fetch`"),
        Root::Blocked(why) => Check::fail(ID, path).why(why.as_str()).fix(
            "Keep courses somewhere you can write: --root <dir>, TER_COURSES_ROOT, or courses_root in ter's config.toml",
        ),
    }
}

/// The board the folder's run is flashed to, or everything on USB when no
/// run names one.
fn boards(facts: &Facts, needs: &Needs) -> Check {
    const ID: &str = "usb.boards";
    let survey = &facts.survey;
    let seen: Vec<String> = survey
        .ports
        .iter()
        .map(|p| format!("{} ({})", p.path, p.kind))
        .chain(
            survey
                .drives
                .iter()
                .map(|d| format!("{} (UF2 drive of {})", d.path.display(), d.board())),
        )
        .collect();
    let all = if seen.is_empty() {
        "no board on USB".to_string()
    } else {
        seen.join(", ")
    };
    // Without the board the run cannot happen; `cargo run` in a project
    // can wait for it.
    let missing = |found: String| {
        if needs.board_required {
            Check::fail(ID, found)
        } else {
            Check::warn(ID, found)
        }
    };
    let Some(tool) = needs.board else {
        return if seen.is_empty() {
            Check::skip(ID, all).why("not needed here")
        } else {
            Check::ok(ID, all)
        };
    };
    if let Tool::Uf2 { family } = tool {
        if let Some(drive) = survey.drives.first() {
            return Check::ok(
                ID,
                format!("{} (UF2 drive of {})", drive.path.display(), drive.board()),
            );
        }
        return match survey.unmounted.first() {
            Some(disk) => missing(format!(
                "{} is a UF2 bootloader that is not mounted",
                disk.display()
            ))
            .fix(format!("udisksctl mount -b {}", disk.display())),
            None => missing(format!("no {} board in its UF2 bootloader", family.name))
                .fix("Plug the board in with a data cable and put it in its bootloader."),
        };
    }
    if let Some(port) = &survey.port_env {
        return if facts.port_env_present {
            Check::ok(ID, format!("{port} ({PORT_ENV})"))
        } else {
            missing(format!("{PORT_ENV} is {port}, and there is no such port"))
                .fix(format!("Plug the board in, or unset {PORT_ENV}."))
        };
    }
    let wanted: Vec<&str> = survey
        .ports
        .iter()
        .filter(|p| tool.wants(p.kind))
        .map(|p| p.path.as_str())
        .collect();
    match wanted.as_slice() {
        [] => missing(format!("no board for {} ({all})", tool.program())).fix(format!(
            "Plug the board in with a data cable, or name its port with {PORT_ENV}."
        )),
        [one] => Check::ok(ID, format!("{one} (flashed with {})", tool.program())),
        many => missing(format!(
            "{} boards for {}: {}",
            many.len(),
            tool.program(),
            many.join(", ")
        ))
        .fix(format!("Set {PORT_ENV} to the one to use.")),
    }
}

fn probes(survey: &Survey) -> Check {
    const ID: &str = "usb.probes";
    let name = |p: &ter_flash::Port| match &p.usb {
        Some(usb) => format!("{} ({usb})", p.path),
        None => p.path.clone(),
    };
    let of = |kind: PortKind| -> Vec<String> {
        survey
            .ports
            .iter()
            .filter(|p| p.kind == kind)
            .map(name)
            .collect()
    };
    let probes = of(PortKind::Probe);
    // An ESP chip's own USB is a JTAG probe too, as probe-rs sees it.
    let esp = of(PortKind::UsbSerialJtag);
    let fix = "Unplug all but the board you are flashing.";
    match (probes.len(), esp.len()) {
        (0, _) => Check::skip(ID, "no debug probe on USB"),
        (1, 0) => Check::ok(ID, probes.join(", ")),
        (1, _) => Check::warn(ID, probes.join(", "))
            .why(format!(
                "probe-rs also sees {} as a probe, and does not pick between probes on its own",
                esp.join(", ")
            ))
            .fix(fix),
        (n, _) => Check::warn(ID, format!("{n} debug probes: {}", probes.join(", ")))
            .why("probe-rs does not pick between probes on its own")
            .fix(fix),
    }
}

/// One access check over several device nodes; `fix` words the way out
/// for one node.
fn access(
    id: &'static str,
    nodes: &[NodeAccess],
    required: bool,
    nothing: &str,
    fix: impl Fn(&NodeAccess) -> String,
) -> Check {
    let Some(denied) = nodes.iter().find(|n| !n.ok) else {
        return if nodes.is_empty() {
            Check::skip(id, nothing)
        } else {
            let all: Vec<&str> = nodes.iter().map(|n| n.what.as_str()).collect();
            Check::ok(id, format!("can use {}", all.join(", ")))
        };
    };
    let found = format!("this login cannot use {} ({})", denied.what, denied.node);
    let check = if required {
        Check::fail(id, found)
    } else {
        Check::warn(id, found)
    };
    check.fix(fix(denied))
}

/// Joining a group, in words, for a node that group owns.
fn group_fix(node: &NodeAccess) -> Option<String> {
    let group = node.group.as_deref()?;
    Some(if node.listed {
        format!(
            "You are in the group {group}, but this login started before that: log out and back in."
        )
    } else {
        format!("sudo usermod -aG {group} $USER, then log out and back in.")
    })
}

fn serial(facts: &Facts, needs: &Needs) -> Check {
    const ID: &str = "linux.serial";
    if !facts.linux {
        return Check::skip(ID, "not looked at").why("only Linux keeps serial ports to a group");
    }
    // A probe's serial port is how a Nucleo's output is read, too.
    let required = needs.board_required && !matches!(needs.board, Some(Tool::Uf2 { .. }));
    access(
        ID,
        &facts.serial,
        required,
        "no board port to look at",
        |n| group_fix(n).unwrap_or_else(|| format!("Check who may use {}: ls -l {0}", n.node)),
    )
}

fn probe_usb(facts: &Facts, needs: &Needs) -> Check {
    const ID: &str = "linux.probe-usb";
    if !facts.linux {
        return Check::skip(ID, "not looked at").why("only Linux needs udev rules for a probe");
    }
    let required = needs.board_required && matches!(needs.board, Some(Tool::ProbeRs { .. }));
    access(
        ID,
        &facts.probes,
        required,
        "no debug probe to look at",
        |_| {
            format!(
                "Install the probe-rs udev rules ({PROBE_SETUP_PAGE}), then unplug the probe and plug it back in."
            )
        },
    )
}

/// Whether the exercise's own run is ready: every failure above stops it,
/// except the two that only `ter new` and `ter ex fetch` care about.
fn venue(folder: &Folder, checks: &[Check]) -> Check {
    const ID: &str = "exercise.venue";
    if folder.kind != Kind::Exercise {
        return Check::skip(ID, "not looked at").why("not in an exercise folder");
    }
    if let Some(why) = &folder.broken {
        return Check::fail(ID, "this exercise cannot be run as it is").why(why.as_str());
    }
    let mode = folder.mode.as_deref().unwrap_or_default();
    let venue = folder.venue.as_deref().unwrap_or_default();
    let tool = folder.tool.as_ref().and_then(|t| t.as_ref().ok());
    let found = match (venue, tool) {
        ("wokwi", _) => "simulation in Wokwi".to_string(),
        ("bench", _) => "hardware on a shared bench".to_string(),
        (_, Some(tool)) if tool.external() => {
            format!("hardware on your board, flashed with {}", tool.program())
        }
        (_, Some(_)) => "hardware on your board, flashed through its UF2 drive".to_string(),
        _ => format!("{mode} on {venue}"),
    };
    let stopped_by: Vec<&str> = checks
        .iter()
        .filter(|c| c.status == Status::Fail)
        .filter(|c| !matches!(c.id, "tool.xiao-generate" | "courses.root"))
        .map(|c| c.id)
        .collect();
    let own_failure = match (venue, &folder.tool, &folder.sim_plan) {
        ("local", Some(Err(why)), _) | ("wokwi", _, Some(Err(why))) => Some(why.as_str()),
        _ => None,
    };
    if let Some(why) = own_failure {
        return Check::fail(ID, found).why(why);
    }
    if !stopped_by.is_empty() {
        return Check::fail(ID, found).why(format!("not ready: {}", stopped_by.join(", ")));
    }
    let note = match (venue, folder.checks) {
        (_, None) => Some("no check.yaml: ter run builds it and grades nothing".to_string()),
        ("bench", _) => Some("`ter bench list` shows the benches you can use".to_string()),
        ("local", Some(total)) if !folder.unseen.is_empty() => Some(format!(
            "{} ({} of {total} checks)",
            graded_only(&folder.unseen, folder.sim_allowed),
            folder.unseen.len()
        )),
        _ => None,
    };
    let ready = Check::ok(ID, found);
    match note {
        Some(note) => ready.why(note),
        None => ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ter_flash::Port;
    use ter_gen::install::Cmd;

    use crate::survey::WokwiToken;

    fn port(path: &str, kind: PortKind) -> Port {
        Port {
            path: path.into(),
            kind,
            usb: Some("0000:0000".into()),
            product: None,
        }
    }

    fn present(name: &'static str, version: &str) -> ToolFact {
        ToolFact {
            name,
            state: State::Present(format!("{version}, /bin/{name}")),
            version: Some(version.into()),
        }
    }

    fn absent(name: &'static str) -> ToolFact {
        ToolFact {
            name,
            state: State::Install {
                why: NOT_INSTALLED.into(),
                run: vec![],
            },
            version: None,
        }
    }

    fn answer() -> Site {
        Site::Answer {
            user: "learner@example.com".into(),
            min_supported: "0.1.0".into(),
            latest: "0.1.3".into(),
            premium: Some(true),
        }
    }

    /// A machine with Rust, an account and nothing else, outside any
    /// folder.
    fn bare() -> Facts {
        Facts {
            cli_version: "0.1.3".into(),
            site_url: "https://learn.example".into(),
            token_source: "keychain",
            site: answer(),
            rustup: Some("/bin/rustup".into()),
            cargo: Some("/bin/cargo".into()),
            toolchains: vec!["stable-x86_64-unknown-linux-gnu (default)".into()],
            targets: Targets::NoProject,
            tools: [
                "espflash",
                "probe-rs",
                "elf2uf2-rs",
                "uf2deploy",
                "xiao-generate",
            ]
            .into_iter()
            .map(absent)
            .collect(),
            survey: Survey::default(),
            port_env_present: false,
            courses_root: "/home/l/ter-courses".into(),
            root: Root::WillCreate,
            linux: true,
            serial: vec![],
            probes: vec![],
            folder: Folder::default(),
            pins: None,
        }
    }

    fn exercise(mode: &str, venue: &str, tool: Tool) -> Folder {
        Folder {
            dir: "/c/ex".into(),
            kind: Kind::Exercise,
            mode: Some(mode.into()),
            venue: Some(venue.into()),
            runner_program: Some(tool.program().into()),
            tool: Some(Ok(tool)),
            sim_plan: (mode == "simulation").then_some(Ok(())),
            sim_allowed: true,
            checks: Some(2),
            ..Folder::default()
        }
    }

    fn c3() -> Tool {
        Tool::Espflash {
            chip: Some("esp32c3".into()),
        }
    }

    fn nucleo() -> Tool {
        Tool::ProbeRs {
            chip: "STM32F401RETx".into(),
        }
    }

    fn installed_targets() -> Targets {
        Targets::Items(vec![Item {
            what: "Rust target riscv32imc-unknown-none-elf (stable)".into(),
            state: State::Present("riscv32imc-unknown-none-elf".into()),
        }])
    }

    fn get<'a>(report: &'a Report, id: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no check {id}"))
    }

    fn failed(report: &Report) -> Vec<&'static str> {
        report
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn every_check_is_reported_once_under_its_stable_id() {
        let report = judge(&bare());
        let ids: Vec<&str> = report.checks.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                "ter.version",
                "site.reach",
                "account.token",
                "account.premium",
                "rust.rustup",
                "rust.cargo",
                "rust.toolchain",
                "rust.targets",
                "tool.espflash",
                "tool.probe-rs",
                "tool.elf2uf2-rs",
                "tool.uf2deploy",
                "tool.xiao-generate",
                "tool.wokwi-cli",
                "wokwi.token",
                "courses.root",
                "usb.boards",
                "usb.probes",
                "linux.serial",
                "linux.probe-usb",
                "exercise.venue",
            ]
        );
        assert_eq!(get(&report, "linux.probe-usb").group, "linux");
        assert_eq!(get(&report, "tool.wokwi-cli").group, "tools");
        assert_eq!(get(&report, "wokwi.token").group, "wokwi");
        assert_eq!(get(&report, "courses.root").group, "disk");
    }

    #[test]
    fn outside_a_folder_missing_tools_and_boards_are_not_problems() {
        let report = judge(&bare());
        assert!(report.ok, "{:?}", failed(&report));
        assert!(report.install.is_empty());
        assert_eq!(report.context.kind, "none");
        let espflash = get(&report, "tool.espflash");
        assert_eq!(espflash.status, Status::Skip);
        assert_eq!(
            espflash.why.as_deref(),
            Some("not needed here; `ter install espflash` installs it")
        );
        for id in [
            "tool.wokwi-cli",
            "wokwi.token",
            "usb.boards",
            "exercise.venue",
        ] {
            assert_eq!(get(&report, id).status, Status::Skip, "{id}");
        }
    }

    #[test]
    fn outside_a_folder_only_ter_the_account_and_rust_can_fail() {
        let mut facts = bare();
        facts.site = Site::NoToken;
        facts.token_source = "none";
        facts.rustup = None;
        facts.cargo = None;
        let report = judge(&facts);
        assert!(!report.ok);
        assert_eq!(
            failed(&report),
            ["account.token", "rust.rustup", "rust.cargo"]
        );
        assert_eq!(
            get(&report, "account.token").fix.as_deref(),
            Some("ter login (or set TER_TOKEN)")
        );
        assert!(
            get(&report, "rust.rustup")
                .fix
                .as_deref()
                .unwrap()
                .contains("https://rustup.rs")
        );
        assert_eq!(get(&report, "rust.toolchain").status, Status::Skip);
        assert_eq!(get(&report, "site.reach").status, Status::Skip);
    }

    #[test]
    fn the_sites_answer_decides_version_reach_token_and_premium() {
        let with = |site: Site| {
            let mut facts = bare();
            facts.site = site;
            judge(&facts)
        };
        let status = |r: &Report| {
            [
                "ter.version",
                "site.reach",
                "account.token",
                "account.premium",
            ]
            .map(|id| get(r, id).status)
        };
        use Status::{Fail, Ok, Skip, Warn};

        assert_eq!(status(&with(answer())), [Ok, Ok, Ok, Ok]);

        let old = with(Site::Answer {
            user: "l".into(),
            min_supported: "0.2.0".into(),
            latest: "0.2.1".into(),
            premium: Some(false),
        });
        assert_eq!(status(&old), [Fail, Ok, Ok, Warn]);
        assert_eq!(
            get(&old, "ter.version").fix.as_deref(),
            Some("ter self-update")
        );

        let behind = with(Site::Answer {
            user: "l".into(),
            min_supported: "0.1.0".into(),
            latest: "0.1.4".into(),
            premium: None,
        });
        assert_eq!(status(&behind), [Warn, Ok, Ok, Skip]);
        assert_eq!(
            get(&behind, "ter.version").why.as_deref(),
            Some("ter 0.1.4 is available")
        );
        assert!(behind.ok, "a warning is not a problem");

        let failed_with = |code: &str| {
            with(Site::Failed {
                code: code.into(),
                message: format!("{code} happened"),
            })
        };
        assert_eq!(
            status(&failed_with("network_error")),
            [Skip, Fail, Skip, Skip]
        );
        assert_eq!(
            status(&failed_with("rate_limited")),
            [Skip, Warn, Skip, Skip]
        );
        assert_eq!(
            status(&failed_with("server_error")),
            [Skip, Fail, Skip, Skip]
        );
        assert_eq!(
            status(&failed_with("token_invalid")),
            [Skip, Ok, Fail, Skip]
        );
        assert_eq!(status(&with(Site::Offline)), [Skip, Skip, Ok, Skip]);
    }

    #[test]
    fn a_refused_token_names_where_it_came_from() {
        let refused = |source: &'static str| {
            let mut facts = bare();
            facts.token_source = source;
            facts.site = Site::Failed {
                code: "token_invalid".into(),
                message: "Token revoked on the Devices page.".into(),
            };
            get(&judge(&facts), "account.token").clone()
        };
        let stored = refused("keychain");
        assert_eq!(stored.fix.as_deref(), Some("ter login"));
        assert_eq!(
            stored.why.as_deref(),
            Some("Token revoked on the Devices page.")
        );
        assert!(
            refused("TER_TOKEN")
                .fix
                .unwrap()
                .starts_with("Unset TER_TOKEN")
        );
    }

    #[test]
    fn a_simulation_exercise_needs_wokwi_and_nothing_on_usb() {
        let mut facts = bare();
        facts.folder = exercise("simulation", "wokwi", c3());
        facts.targets = installed_targets();
        let report = judge(&facts);
        assert_eq!(
            failed(&report),
            ["tool.wokwi-cli", "wokwi.token", "exercise.venue"]
        );
        assert_eq!(report.install, ["wokwi-cli"]);
        assert_eq!(get(&report, "tool.espflash").status, Status::Skip);
        assert_eq!(get(&report, "usb.boards").status, Status::Skip);
        assert_eq!(
            get(&report, "exercise.venue").why.as_deref(),
            Some("not ready: tool.wokwi-cli, wokwi.token")
        );

        facts.survey.wokwi_cli = Some("/bin/wokwi-cli".into());
        facts.survey.wokwi_token = Some(WokwiToken {
            source: "keychain",
            well_formed: true,
        });
        let report = judge(&facts);
        assert!(report.ok, "{:?}", failed(&report));
        assert_eq!(get(&report, "exercise.venue").found, "simulation in Wokwi");

        // A token of the wrong shape is a warning: Wokwi is not asked.
        facts.survey.wokwi_token = Some(WokwiToken {
            source: "WOKWI_CLI_TOKEN",
            well_formed: false,
        });
        let report = judge(&facts);
        assert!(report.ok);
        assert_eq!(get(&report, "wokwi.token").status, Status::Warn);

        // A circuit Wokwi cannot run the checks in stops the run.
        facts.folder.sim_plan = Some(Err("No Wokwi circuit for this exercise.".into()));
        let venue = get(&judge(&facts), "exercise.venue").clone();
        assert_eq!(venue.status, Status::Fail);
        assert_eq!(
            venue.why.as_deref(),
            Some("No Wokwi circuit for this exercise.")
        );
    }

    #[test]
    fn a_hardware_exercise_needs_its_tool_its_target_and_its_board() {
        let mut facts = bare();
        facts.folder = exercise("hardware", "local", c3());
        facts.targets = Targets::Items(vec![Item {
            what: "Rust target riscv32imc-unknown-none-elf (stable)".into(),
            state: State::Install {
                why: NOT_INSTALLED.into(),
                run: vec![Cmd {
                    program: "rustup".into(),
                    args: vec![],
                }],
            },
        }]);
        let report = judge(&facts);
        assert_eq!(
            failed(&report),
            [
                "rust.targets",
                "tool.espflash",
                "usb.boards",
                "exercise.venue"
            ]
        );
        assert_eq!(report.install, ["targets", "espflash"]);
        assert_eq!(get(&report, "tool.probe-rs").status, Status::Skip);
        assert_eq!(get(&report, "tool.wokwi-cli").status, Status::Skip);
        assert_eq!(
            get(&report, "rust.targets").fix.as_deref(),
            Some("ter install targets")
        );
        assert_eq!(report.context.mode.as_deref(), Some("hardware"));
        assert_eq!(report.context.venue.as_deref(), Some("local"));
    }

    #[test]
    fn a_board_this_login_cannot_open_says_how_to_join_its_group() {
        let mut facts = bare();
        facts.folder = exercise("hardware", "local", c3());
        facts.folder.unseen = vec!["blink-rate".into()];
        facts.targets = installed_targets();
        facts.tools[0] = present("espflash", "4.6.0");
        facts.survey.ports = vec![port("/dev/ttyACM0", PortKind::UsbSerialJtag)];
        let denied = |listed: bool| NodeAccess {
            what: "/dev/ttyACM0".into(),
            node: "/dev/ttyACM0".into(),
            ok: false,
            group: Some("dialout".into()),
            listed,
        };

        facts.serial = vec![denied(false)];
        let report = judge(&facts);
        assert_eq!(failed(&report), ["linux.serial", "exercise.venue"]);
        assert_eq!(
            get(&report, "linux.serial").fix.as_deref(),
            Some("sudo usermod -aG dialout $USER, then log out and back in.")
        );
        assert!(report.install.is_empty(), "ter install cannot fix a group");

        // In the group already, but not in this login.
        facts.serial = vec![denied(true)];
        let fix = get(&judge(&facts), "linux.serial").fix.clone().unwrap();
        assert!(fix.starts_with("You are in the group dialout"), "{fix}");

        facts.serial[0].ok = true;
        let report = judge(&facts);
        assert!(report.ok, "{:?}", failed(&report));
        let venue = get(&report, "exercise.venue");
        assert_eq!(venue.found, "hardware on your board, flashed with espflash");
        assert_eq!(
            venue.why.as_deref(),
            Some("blink-rate is graded only in simulation (1 of 2 checks)")
        );

        // Off Linux the access checks are not made at all.
        facts.linux = false;
        facts.serial = vec![denied(false)];
        let report = judge(&facts);
        assert!(report.ok);
        assert_eq!(get(&report, "linux.serial").status, Status::Skip);
    }

    #[test]
    fn a_nucleo_exercise_needs_probe_rs_a_probe_and_usb_access_to_it() {
        let mut facts = bare();
        facts.folder = exercise("hardware", "local", nucleo());
        facts.targets = installed_targets();
        facts.tools[0] = present("espflash", "4.6.0");
        // An ESP board is plugged in, which probe-rs does not flash.
        facts.survey.ports = vec![port("/dev/ttyACM0", PortKind::UsbSerialJtag)];
        let report = judge(&facts);
        assert_eq!(
            failed(&report),
            ["tool.probe-rs", "usb.boards", "exercise.venue"]
        );
        assert_eq!(report.install, ["probe-rs"]);

        facts.tools[1] = present("probe-rs", "0.32.0");
        facts
            .survey
            .ports
            .push(port("/dev/ttyACM3", PortKind::Probe));
        facts.probes = vec![NodeAccess {
            what: "debug probe 0483:374b".into(),
            node: "/dev/bus/usb/007/002".into(),
            ok: false,
            group: Some("root".into()),
            listed: false,
        }];
        let report = judge(&facts);
        assert_eq!(failed(&report), ["linux.probe-usb", "exercise.venue"]);
        let fix = get(&report, "linux.probe-usb").fix.clone().unwrap();
        assert!(
            fix.contains(PROBE_SETUP_PAGE) && fix.contains("unplug"),
            "{fix}"
        );
        assert_eq!(
            get(&report, "usb.boards").found,
            "/dev/ttyACM3 (flashed with probe-rs)"
        );

        facts.probes[0].ok = true;
        assert!(judge(&facts).ok);
    }

    #[test]
    fn two_probes_are_a_warning_and_two_boards_need_ter_port() {
        let mut facts = bare();
        facts.survey.ports = vec![
            port("/dev/ttyACM1", PortKind::Probe),
            port("/dev/ttyACM2", PortKind::Probe),
        ];
        let report = judge(&facts);
        assert!(report.ok);
        let probes = get(&report, "usb.probes");
        assert_eq!(probes.status, Status::Warn);
        assert!(probes.found.starts_with("2 debug probes"));

        // One probe and an ESP board: probe-rs sees two probes.
        let mut mixed = bare();
        mixed.survey.ports = vec![
            port("/dev/ttyACM0", PortKind::UsbSerialJtag),
            port("/dev/ttyACM3", PortKind::Probe),
        ];
        let both = get(&judge(&mixed), "usb.probes").clone();
        assert_eq!(both.status, Status::Warn);
        assert!(both.why.unwrap().contains("/dev/ttyACM0"));
        mixed.survey.ports.pop();
        assert_eq!(get(&judge(&mixed), "usb.probes").status, Status::Skip);

        facts.folder = exercise("hardware", "local", nucleo());
        facts.targets = installed_targets();
        facts.tools[1] = present("probe-rs", "0.32.0");
        let report = judge(&facts);
        let boards = get(&report, "usb.boards");
        assert_eq!(boards.status, Status::Fail);
        assert_eq!(
            boards.fix.as_deref(),
            Some("Set TER_PORT to the one to use.")
        );

        facts.survey.port_env = Some("/dev/ttyACM2".into());
        facts.port_env_present = true;
        assert_eq!(get(&judge(&facts), "usb.boards").status, Status::Ok);
        facts.port_env_present = false;
        assert_eq!(get(&judge(&facts), "usb.boards").status, Status::Fail);
    }

    #[test]
    fn a_generator_below_the_floor_fails_everywhere() {
        let mut facts = bare();
        facts.tools[4] = ToolFact {
            name: "xiao-generate",
            state: State::Install {
                why: "xiao-generate 0.1.0 is too old: ter needs 0.1.1 or newer".into(),
                run: vec![],
            },
            version: Some("0.1.0".into()),
        };
        // An espflash that is too old is only a warning where no run
        // needs it.
        facts.tools[0] = ToolFact {
            name: "espflash",
            state: State::Install {
                why: "espflash 3.3.0 is too old: ter needs 4.0.0 or newer".into(),
                run: vec![],
            },
            version: Some("3.3.0".into()),
        };
        let report = judge(&facts);
        assert_eq!(failed(&report), ["tool.xiao-generate"]);
        assert_eq!(report.install, ["xiao-generate"]);
        assert_eq!(get(&report, "tool.espflash").status, Status::Warn);

        // It does not stop an exercise's run: the scaffold came from the
        // site.
        facts.folder = exercise("simulation", "wokwi", c3());
        facts.targets = installed_targets();
        facts.survey.wokwi_cli = Some("/bin/wokwi-cli".into());
        facts.survey.wokwi_token = Some(WokwiToken {
            source: "file",
            well_formed: true,
        });
        let report = judge(&facts);
        assert_eq!(failed(&report), ["tool.xiao-generate"]);
        assert_eq!(get(&report, "exercise.venue").status, Status::Ok);
    }

    #[test]
    fn dev_warns_when_a_tool_is_not_the_curriculums_version() {
        let mut facts = bare();
        facts.tools[0] = present("espflash", "4.4.0");
        facts.tools[1] = present("probe-rs", "0.32.0");
        facts.tools[4] = present("xiao-generate", "0.1.2");
        assert_eq!(get(&judge(&facts), "tool.xiao-generate").status, Status::Ok);

        facts.pins = Some(Pins {
            xiao_generate: Some("0.1.1".into()),
            espflash: Some("4.5".into()),
            probe_rs: Some("0.32".into()),
        });
        let report = judge(&facts);
        assert!(report.ok);
        let generator = get(&report, "tool.xiao-generate");
        assert_eq!(generator.status, Status::Warn);
        assert_eq!(
            generator.why.as_deref(),
            Some("the curriculum is checked with xiao-generate 0.1.1")
        );
        assert_eq!(
            generator.fix.as_deref(),
            Some("cargo install xiao-generate --locked --force --version 0.1.1")
        );
        assert_eq!(get(&report, "tool.espflash").status, Status::Warn);
        assert_eq!(get(&report, "tool.probe-rs").status, Status::Ok);

        facts.tools[4] = present("xiao-generate", "0.1.1");
        assert_eq!(get(&judge(&facts), "tool.xiao-generate").status, Status::Ok);
    }

    #[test]
    fn a_project_wants_its_runners_program_and_waits_for_the_board() {
        let mut facts = bare();
        facts.folder = Folder {
            dir: "/p".into(),
            kind: Kind::Project,
            tool: Some(Ok(nucleo())),
            runner_program: Some("probe-rs".into()),
            ..Folder::default()
        };
        facts.targets = installed_targets();
        let report = judge(&facts);
        assert_eq!(failed(&report), ["tool.probe-rs"]);
        assert_eq!(get(&report, "usb.boards").status, Status::Warn);
        assert_eq!(get(&report, "exercise.venue").status, Status::Skip);
        assert_eq!(report.context.kind, "project");
    }

    #[test]
    fn a_bench_exercise_needs_nothing_on_this_machines_usb() {
        let mut facts = bare();
        facts.folder = exercise("hardware", "bench", c3());
        facts.targets = installed_targets();
        let report = judge(&facts);
        assert!(report.ok, "{:?}", failed(&report));
        assert_eq!(
            get(&report, "exercise.venue").found,
            "hardware on a shared bench"
        );
    }

    #[test]
    fn a_folder_that_cannot_be_read_or_written_fails_its_own_check() {
        let mut facts = bare();
        facts.folder = Folder {
            kind: Kind::Exercise,
            broken: Some("ter.toml is not valid.".into()),
            ..Folder::default()
        };
        facts.targets = Targets::Unreadable("no .cargo/config.toml".into());
        facts.root = Root::Blocked("/srv is read-only".into());
        let report = judge(&facts);
        assert_eq!(
            failed(&report),
            ["rust.targets", "courses.root", "exercise.venue"]
        );
        assert!(report.install.is_empty());
        assert!(
            get(&report, "courses.root")
                .fix
                .as_deref()
                .unwrap()
                .contains("TER_COURSES_ROOT")
        );
    }

    #[test]
    fn a_uf2_board_is_looked_for_as_a_drive() {
        let mut facts = bare();
        facts.folder = exercise(
            "hardware",
            "local",
            Tool::Uf2 {
                family: &ter_flash::uf2::RP2040,
            },
        );
        facts.targets = installed_targets();
        let report = judge(&facts);
        assert_eq!(failed(&report), ["usb.boards", "exercise.venue"]);
        assert_eq!(
            get(&report, "tool.elf2uf2-rs").status,
            Status::Skip,
            "ter flashes UF2 boards itself"
        );

        facts.survey.unmounted = vec!["/dev/sda1".into()];
        assert_eq!(
            get(&judge(&facts), "usb.boards").fix.as_deref(),
            Some("udisksctl mount -b /dev/sda1")
        );
    }
}
