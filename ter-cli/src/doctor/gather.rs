//! Looking at the machine for `ter doctor`. Nothing here changes it,
//! builds, flashes, opens a serial port or talks to a chip; the site is
//! asked one `ping`, and Wokwi nothing.

use std::path::{Path, PathBuf};

use ter_check::CheckFile;
use ter_flash::{Port, PortKind, Tool};
use ter_gen::install::{self as plan, Probe, System};
use ter_sdk::project::{self, TerToml};
use ter_telemetry::local::Local;

use super::Args;
use super::judge::{
    Facts, Folder, Kind, NodeAccess, Pins, Root, Sim86, Sim86Account, Sim86Install, Site, Targets,
    ToolFact,
};
use crate::config::Config;
use crate::output::CliError;
use crate::session::Session;
use crate::survey::{Survey, survey};
use crate::{hw, run, sim};

/// The cargo-installed tools doctor reports, in `ter install`'s order.
const CARGO_TOOLS: [(&str, plan::Tool); 5] = [
    ("xiao-generate", plan::Tool::XiaoGenerate),
    ("espflash", plan::Tool::Espflash),
    ("probe-rs", plan::Tool::ProbeRs),
    ("elf2uf2-rs", plan::Tool::Elf2uf2Rs),
    ("uf2deploy", plan::Tool::Uf2deploy),
];

pub async fn gather(args: &Args) -> Result<Facts, CliError> {
    let start = match &args.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()
            .map_err(|e| CliError::new("io_error", format!("No current folder: {e}")))?,
    };
    let pins = args.dev.as_deref().map(read_pins).transpose()?;
    let session = Session::open()?;
    let config = Config::load()?;

    let site = if args.offline {
        Site::Offline
    } else if session.token_source == "none" {
        Site::NoToken
    } else {
        // The one site call. A token the site refuses is reported and
        // left where it is.
        match session.client.ping().await {
            Ok(ping) => Site::Answer {
                user: ping.user.clone(),
                min_supported: ping.min_supported_version.clone(),
                latest: ping.latest_version.clone(),
                premium: ping.premium,
            },
            Err(e) => Site::Failed {
                code: e.code().to_string(),
                message: e.to_string(),
            },
        }
    };

    let survey = survey();
    let folder = folder(&start, &survey);
    let rustup = plan::find_program("rustup");
    let toolchains = rustup
        .as_deref()
        .and_then(|r| System.output(r, &["toolchain", "list"]))
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("no installed"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let targets = match folder.kind {
        Kind::None => Targets::NoProject,
        _ => match plan::read_project(&folder.dir) {
            Ok(project) => Targets::Items(plan::plan(&System, plan::Tool::Targets, Some(&project))),
            Err(why) => Targets::Unreadable(why),
        },
    };
    let mut tools: Vec<ToolFact> = CARGO_TOOLS
        .into_iter()
        .filter_map(|(name, tool)| {
            let state = plan::plan(&System, tool, None).pop()?.state;
            // The version is only compared with a curriculum's pins.
            let version = pins.as_ref().and_then(|_| plan::installed(&System, tool));
            Some(ToolFact {
                name,
                state,
                version,
            })
        })
        .collect();
    // Reported with the flashing tools first, the generator last.
    tools.rotate_left(1);

    let port_env_present = survey
        .port_env
        .as_deref()
        .is_some_and(ter_flash::port::present);
    let display = |p: PathBuf| p.display().to_string();
    Ok(Facts {
        cli_version: session.client.cli_version().to_string(),
        site_url: session.client.site_url().to_string(),
        token_source: session.token_source,
        site,
        rustup: rustup.map(display),
        cargo: plan::find_program("cargo").map(display),
        toolchains,
        targets,
        tools,
        port_env_present,
        root: root(&config.courses_root),
        courses_root: config.courses_root,
        linux: cfg!(target_os = "linux"),
        sim86: sim86(args.offline),
        serial: serial_access(&survey, port_env_present),
        probes: probe_access(&survey),
        survey,
        folder,
        pins,
    })
}

/// The sim86 package in ter's tools folder, and, unless `offline`, what
/// sim86 says of the account: one call, which spends no credits.
fn sim86(offline: bool) -> Sim86 {
    use ter_telemetry::sim86::FailureKind;
    let command = crate::sim86::home()
        .map(|home| crate::sim86::command_in(&home))
        .unwrap_or_default();
    let Some(helper) = crate::sim86::installed() else {
        return Sim86 {
            command,
            ..Sim86::default()
        };
    };
    let install = match helper.version() {
        Ok(found) => Sim86Install::Found {
            version: found.sim86.unwrap_or_default(),
            python: found.python.unwrap_or_default(),
        },
        Err(e) => Sim86Install::Broken(e.explain()),
    };
    // A package too old for ter is upgraded before its account matters.
    let usable = matches!(&install, Sim86Install::Found { version, .. } if !ter_telemetry::sim86::too_old(version));
    let account = if offline || !usable {
        Sim86Account::NotAsked
    } else {
        match helper.balance() {
            Ok(balance) => Sim86Account::Balance {
                plan: balance.plan,
                credits: balance.credits,
            },
            Err(e) if e.kind == FailureKind::NoKey => Sim86Account::NoKey,
            Err(e) if e.kind == FailureKind::Auth => Sim86Account::Refused,
            Err(e) => Sim86Account::Unknown(e.message.lines().next().unwrap_or_default().into()),
        }
    };
    Sim86 {
        install,
        account,
        command,
    }
}

/// What `dir` is and what its run needs, without building it.
fn folder(start: &Path, survey: &Survey) -> Folder {
    let Some(dir) = project::enclosing_exercise(start) else {
        if !start.join(".cargo/config.toml").is_file() {
            return Folder {
                dir: start.to_path_buf(),
                ..Folder::default()
            };
        }
        let runner = std::fs::read_to_string(start.join(".cargo/config.toml"))
            .ok()
            .and_then(|t| project::find_runner(&t));
        return Folder {
            dir: start.to_path_buf(),
            kind: Kind::Project,
            tool: runner.as_deref().map(|r| {
                Tool::from_runner(r).map_err(|e| format!("ter cannot flash this board: {e}."))
            }),
            runner_program: runner.as_deref().and_then(runner_program),
            ..Folder::default()
        };
    };
    let mut folder = Folder {
        kind: Kind::Exercise,
        dir: dir.clone(),
        ..Folder::default()
    };
    let ter = match TerToml::load(&dir) {
        Ok(ter) => ter,
        Err(e) => {
            folder.broken = Some(e.message);
            return folder;
        }
    };
    folder.sim_allowed = ter.modes.iter().any(|m| m == "simulation");
    if !ter.modes.contains(&ter.mode) {
        folder.broken = Some(format!(
            "ter.toml's mode is {}, and {} allows {}.",
            ter.mode,
            ter.exercise,
            ter.modes.join(" and ")
        ));
        return folder;
    }
    folder.mode = Some(ter.mode.clone());
    let venue = match run::venue_for(&ter.mode, ter.venue.as_deref(), &dir) {
        Ok(venue) => venue,
        Err(e) => {
            folder.broken = Some(e.message);
            return folder;
        }
    };
    folder.venue = Some(venue.to_string());
    let tool = hw::tool(&dir).map_err(|e| e.message);
    folder.runner_program = tool.as_ref().ok().map(|t| t.program().to_string());

    let check_path = dir.join(run::CHECK_YAML);
    if check_path.is_file() {
        let file = std::fs::read_to_string(&check_path)
            .map_err(|e| format!("Could not read {}: {e}", check_path.display()))
            .and_then(|text| {
                CheckFile::parse(&text)
                    .map_err(|e| format!("{} is not a valid check: {e}", check_path.display()))
            });
        match file {
            Ok(file) => {
                folder.checks = Some(file.checks.len());
                if venue == "wokwi" {
                    folder.sim_plan = Some(
                        sim::plan(&dir, &ter, &file)
                            .map(|_| ())
                            .map_err(|e| e.message),
                    );
                }
                if venue == "sim86" {
                    folder.sim_plan = Some(
                        crate::sim86::plan(&dir, &ter, &file)
                            .map(|_| ())
                            .map_err(|e| e.message),
                    );
                }
                if let Ok(tool) = &tool {
                    // What a bare board on the port this tool would use
                    // shows.
                    let kind = survey
                        .ports
                        .iter()
                        .find(|p| tool.wants(p.kind))
                        .map(|p| p.kind)
                        .unwrap_or_default();
                    folder.unseen = sim::unobservable(&file, &Local::provides_on(tool, kind))
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect();
                }
            }
            Err(why) => folder.broken = Some(why),
        }
    }
    folder.tool = Some(tool);
    folder
}

/// The program a runner starts, when it is one `ter install` knows.
fn runner_program(runner: &str) -> Option<String> {
    let program = runner.split_whitespace().next()?;
    let name = Path::new(program).file_stem()?.to_str()?;
    CARGO_TOOLS
        .iter()
        .any(|(tool, _)| *tool == name)
        .then(|| name.to_string())
}

/// The tool versions in a curriculum checkout's `toolchain.yaml`.
fn read_pins(curriculum: &Path) -> Result<Pins, CliError> {
    let path = curriculum.join("toolchain.yaml");
    let bad =
        |why: String| CliError::new("bad_curriculum", format!("--dev: {} {why}", path.display()));
    let text = std::fs::read_to_string(&path).map_err(|e| bad(format!("cannot be read: {e}")))?;
    parse_pins(&text).map_err(|e| bad(format!("is not valid: {e}")))
}

fn parse_pins(yaml: &str) -> Result<Pins, String> {
    let file: serde_json::Value = serde_norway::from_str(yaml).map_err(|e| e.to_string())?;
    let pin = |name: &str| match &file["tools"][name] {
        serde_json::Value::String(v) => Some(v.clone()),
        serde_json::Value::Number(v) => Some(v.to_string()),
        _ => None,
    };
    Ok(Pins {
        xiao_generate: pin("xiao-generate"),
        espflash: pin("espflash"),
        probe_rs: pin("probe-rs"),
    })
}

/// Whether courses can be kept at `root`: it is a folder this login can
/// write in, or one can be made there.
fn root(root: &Path) -> Root {
    if root.is_dir() {
        return if can_write_in(root) {
            Root::Exists
        } else {
            Root::Blocked(format!("{} cannot be written to", root.display()))
        };
    }
    if root.exists() {
        return Root::Blocked(format!("{} is not a folder", root.display()));
    }
    match root.ancestors().skip(1).find(|a| a.exists()) {
        Some(parent) if parent.is_dir() && can_write_in(parent) => Root::WillCreate,
        Some(parent) => Root::Blocked(format!(
            "it does not exist, and {} cannot be written to",
            parent.display()
        )),
        None => Root::Blocked("it does not exist, and nothing above it does".into()),
    }
}

#[cfg(unix)]
fn can_write_in(dir: &Path) -> bool {
    use rustix::fs::{Access, access};
    access(dir, Access::WRITE_OK | Access::EXEC_OK).is_ok()
}

#[cfg(not(unix))]
fn can_write_in(dir: &Path) -> bool {
    std::fs::metadata(dir).is_ok_and(|m| !m.permissions().readonly())
}

/// Whether this login may use each board's and probe's serial port. Asked
/// of the system; the port is not opened.
fn serial_access(survey: &Survey, port_env_present: bool) -> Vec<NodeAccess> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let named = survey
        .port_env
        .as_deref()
        .filter(|p| port_env_present && !survey.ports.iter().any(|q| q.path == *p));
    survey
        .ports
        .iter()
        .map(|p| p.path.as_str())
        .chain(named)
        .map(|path| node_access(path.to_string(), Path::new(path)))
        .collect()
}

/// Whether this login may use each probe's own USB device, which is what
/// probe-rs flashes through (not its serial port).
fn probe_access(survey: &Survey) -> Vec<NodeAccess> {
    survey
        .ports
        .iter()
        .filter(|p| p.kind == PortKind::Probe)
        .filter_map(|p| {
            let node = usb_node(p)?;
            let what = match &p.usb {
                Some(usb) => format!("debug probe {usb}"),
                None => "debug probe".to_string(),
            };
            Some(node_access(what, &node))
        })
        .collect()
}

/// The USB device behind a serial port, as its node under `/dev/bus/usb`,
/// from sysfs: the port's device is an interface of it.
fn usb_node(port: &Port) -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let name = Path::new(&port.path).file_name()?;
    let interface = Path::new("/sys/class/tty")
        .join(name)
        .join("device")
        .canonicalize()
        .ok()?;
    let number = |dir: &Path, file: &str| -> Option<u32> {
        std::fs::read_to_string(dir.join(file))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    interface.ancestors().find_map(|dir| {
        let (bus, device) = (number(dir, "busnum")?, number(dir, "devnum")?);
        Some(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{device:03}")))
    })
}

#[cfg(unix)]
fn node_access(what: String, node: &Path) -> NodeAccess {
    use rustix::fs::{Access, access};
    use std::os::unix::fs::MetadataExt;

    let ok = access(node, Access::READ_OK | Access::WRITE_OK).is_ok();
    let read = |file: &str| std::fs::read_to_string(file).unwrap_or_default();
    let group = std::fs::metadata(node)
        .ok()
        .and_then(|m| group_named(&read("/etc/group"), m.gid()));
    let user = user_named(&read("/etc/passwd"), rustix::process::getuid().as_raw());
    let listed = match (&group, &user) {
        (Some((_, members)), Some(user)) => members.contains(user),
        _ => false,
    };
    NodeAccess {
        what,
        node: node.display().to_string(),
        ok,
        group: group.map(|(name, _)| name),
        listed,
    }
}

#[cfg(not(unix))]
fn node_access(what: String, node: &Path) -> NodeAccess {
    NodeAccess {
        what,
        node: node.display().to_string(),
        ok: true,
        group: None,
        listed: false,
    }
}

/// The group with id `gid` in `/etc/group`'s text: its name and members.
#[cfg_attr(not(unix), allow(dead_code))]
fn group_named(etc_group: &str, gid: u32) -> Option<(String, Vec<String>)> {
    etc_group.lines().find_map(|line| {
        let mut fields = line.split(':');
        let (name, _, id) = (fields.next()?, fields.next()?, fields.next()?);
        (id.trim().parse() == Ok(gid)).then(|| {
            let members = fields
                .next()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect();
            (name.to_string(), members)
        })
    })
}

/// The account with id `uid` in `/etc/passwd`'s text.
#[cfg_attr(not(unix), allow(dead_code))]
fn user_named(etc_passwd: &str, uid: u32) -> Option<String> {
    etc_passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let (name, _, id) = (fields.next()?, fields.next()?, fields.next()?);
        (id.trim().parse() == Ok(uid)).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nodes_group_and_its_members_come_from_etc_group() {
        let groups = "root:x:0:\ndialout:x:20:ada,grace\nplugdev:x:46:ada\nbroken\n";
        assert_eq!(
            group_named(groups, 20),
            Some(("dialout".into(), vec!["ada".into(), "grace".into()]))
        );
        assert_eq!(group_named(groups, 0), Some(("root".into(), vec![])));
        assert_eq!(
            group_named(groups, 986),
            None,
            "uucp is not on this machine"
        );
        let users = "root:x:0:0:root:/root:/bin/sh\nada:x:1000:1000::/home/ada:/bin/sh\n";
        assert_eq!(user_named(users, 1000).as_deref(), Some("ada"));
        assert_eq!(user_named(users, 1001), None);
    }

    #[test]
    fn pins_are_read_from_the_curriculums_toolchain_file() {
        let yaml = "rust:\n  channel: stable\ntools:\n  espflash: \"4.5\"\n  probe-rs: 0.32\n  xiao-generate: \"0.1.1\"\n";
        assert_eq!(
            parse_pins(yaml).unwrap(),
            Pins {
                xiao_generate: Some("0.1.1".into()),
                espflash: Some("4.5".into()),
                probe_rs: Some("0.32".into()),
            }
        );
        assert_eq!(parse_pins("rust: {}\n").unwrap(), Pins::default());
        assert!(parse_pins("tools: [").is_err());
    }

    #[test]
    fn only_runners_ter_installs_are_named() {
        assert_eq!(
            runner_program("probe-rs run --chip STM32F401RETx").as_deref(),
            Some("probe-rs")
        );
        assert_eq!(
            runner_program("/home/a/.cargo/bin/espflash flash --monitor").as_deref(),
            Some("espflash")
        );
        assert_eq!(
            runner_program("elf2uf2-rs -d").as_deref(),
            Some("elf2uf2-rs")
        );
        assert_eq!(runner_program("qemu-system-arm -M lm3s6965evb"), None);
        assert_eq!(runner_program(""), None);
    }

    #[test]
    fn the_courses_root_may_be_there_or_makeable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(root(dir.path()), Root::Exists);
        assert_eq!(root(&dir.path().join("a/b/courses")), Root::WillCreate);
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(matches!(root(&file), Root::Blocked(_)));
        assert!(matches!(root(&file.join("courses")), Root::Blocked(_)));
    }

    #[test]
    fn a_plain_folder_a_project_and_an_exercise_are_told_apart() {
        let survey = Survey::default();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(folder(dir.path(), &survey).kind, Kind::None);

        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[target.thumbv7em-none-eabihf]\nrunner = \"probe-rs run --chip STM32F401RETx\"\n",
        )
        .unwrap();
        let project = folder(dir.path(), &survey);
        assert_eq!(project.kind, Kind::Project);
        assert_eq!(project.runner_program.as_deref(), Some("probe-rs"));
        assert!(matches!(project.tool, Some(Ok(Tool::ProbeRs { .. }))));

        std::fs::write(
            dir.path().join("ter.toml"),
            "exercise = \"e\"\ncourse = \"c\"\ntarget = \"nucleo-f401re-nostd\"\nmodes = [\"hardware\"]\nmode = \"hardware\"\nfetched_sha256 = \"\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let exercise = folder(&dir.path().join("src"), &survey);
        assert_eq!(exercise.kind, Kind::Exercise);
        assert_eq!(exercise.dir, dir.path());
        assert_eq!(exercise.mode.as_deref(), Some("hardware"));
        assert_eq!(exercise.venue.as_deref(), Some("local"));
        assert_eq!(exercise.checks, None);
        assert!(!exercise.sim_allowed);

        std::fs::write(dir.path().join("ter.toml"), "exercise = 3\n").unwrap();
        assert!(folder(dir.path(), &survey).broken.is_some());
    }
}
