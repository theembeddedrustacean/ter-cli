//! The boards ter generates projects for itself, without xiao-generate.
//!
//! xiao-generate covers the XIAO boards. A board it does not know (the
//! Nucleo-F401RE of the STM32 courses) gets a template here, built into
//! the `ter` binary: a project that builds and flashes as written, with the
//! crates and versions the curriculum's exercises for that board use.

use std::path::{Path, PathBuf};

use crate::new::{Listing, NewArgs};

/// A board ter generates itself.
#[derive(Debug)]
pub struct Board {
    /// The name `ter new --board` takes.
    pub id: &'static str,
    /// For people.
    pub name: &'static str,
    pub chip: &'static str,
    /// Its one HAL, the default.
    pub hal: &'static str,
    /// Its one template, the default.
    pub template: &'static str,
    /// The tool its runner flashes with.
    pub flashing: &'static str,
    /// The project's files: path in the project, contents. `{{name}}`
    /// is replaced by the crate name.
    files: &'static [(&'static str, &'static str)],
}

const NAME: &str = "{{name}}";

pub const NUCLEO_F401RE: Board = Board {
    id: "nucleo-f401re",
    name: "Nucleo-F401RE",
    chip: "STM32F401RE",
    hal: "stm32f4xx-hal",
    template: "blinky",
    flashing: "probe-rs",
    files: &[
        (
            "Cargo.toml",
            include_str!("../templates/nucleo-f401re/Cargo.toml.tmpl"),
        ),
        (
            ".cargo/config.toml",
            include_str!("../templates/nucleo-f401re/cargo-config.toml"),
        ),
        (
            "rust-toolchain.toml",
            include_str!("../templates/nucleo-f401re/rust-toolchain.toml"),
        ),
        (
            "memory.x",
            include_str!("../templates/nucleo-f401re/memory.x"),
        ),
        (
            "build.rs",
            include_str!("../templates/nucleo-f401re/build.rs.txt"),
        ),
        (
            "src/main.rs",
            include_str!("../templates/nucleo-f401re/main.rs.txt"),
        ),
    ],
};

/// Every board ter generates itself.
pub const BOARDS: &[Board] = &[NUCLEO_F401RE];

/// The board, when ter generates it itself.
pub fn find(id: &str) -> Option<&'static Board> {
    BOARDS.iter().find(|b| b.id == id)
}

/// Cargo's rule for a package name: letters, digits, `-` and `_`, not
/// starting with a digit.
fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a crate name: use letters, digits, `-` and `_`, starting with a letter."
        ))
    }
}

impl Board {
    /// Refuse what this template does not offer, before anything is
    /// written. `--yes` and `--no-install` are accepted: it installs
    /// nothing.
    pub fn check(&self, args: &NewArgs) -> Result<(), String> {
        check_name(&args.name)?;
        let only = |what: &str, given: &Option<String>, offered: &str| match given {
            Some(g) if g != offered => Err(format!(
                "The {} template has {what} `{offered}` only, not `{g}`.",
                self.name
            )),
            _ => Ok(()),
        };
        only("the HAL", &args.hal, self.hal)?;
        only("the template", &args.template, self.template)?;
        only("the flashing tool", &args.flashing, self.flashing)?;
        only("logging", &args.logging, "none")?;
        if args.r#async {
            return Err(format!(
                "The {} template is blocking code only; leave out --async.",
                self.name
            ));
        }
        if !args.extra.is_empty() {
            return Err(format!(
                "The {} template is ter's own, not xiao-generate's, so it takes no flags after `--`.",
                self.name
            ));
        }
        Ok(())
    }

    /// The project's files for crate `name`, relative to its folder.
    pub fn files(&self, name: &str) -> Vec<(PathBuf, String)> {
        self.files
            .iter()
            .map(|(path, text)| (PathBuf::from(path), text.replace(NAME, name)))
            .collect()
    }

    /// Write the project to `args.out_dir()`, which must be missing or
    /// empty. Returns the folder.
    pub fn write(&self, args: &NewArgs) -> Result<PathBuf, String> {
        self.check(args)?;
        let dir = args.out_dir();
        let occupied = std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some());
        if occupied {
            return Err(format!(
                "{} already exists and is not empty; pick another --name or --out.",
                dir.display()
            ));
        }
        for (path, text) in self.files(&args.name) {
            write_file(&dir.join(path), &text)?;
        }
        Ok(dir)
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("Could not write {}: {e}", path.display()))
}

/// ter's part of a listing: its own boards, or one board's HAL or
/// template. `None` when `board` is not one of ter's.
pub fn listing(listing: Listing, board: Option<&str>) -> Option<String> {
    let line = |b: &Board| match listing {
        Listing::Boards => format!("  {:<16}{} ({})", b.id, b.name, b.chip),
        Listing::Hals => format!("  {:<16}{} (default)", b.id, b.hal),
        Listing::Templates => format!("  {:<16}{} (default)", b.id, b.template),
    };
    match board {
        // One board: xiao-generate's layout, the default marked.
        Some(id) => find(id).map(|b| {
            let item = match listing {
                Listing::Boards => return format!("{}\n", line(b)),
                Listing::Hals => b.hal,
                Listing::Templates => b.template,
            };
            format!("  {item} [default]\n")
        }),
        None => {
            let mut text = String::from("Boards ter generates itself:\n");
            for b in BOARDS {
                text.push_str(&line(b));
                text.push('\n');
            }
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(name: &str) -> NewArgs {
        NewArgs {
            board: "nucleo-f401re".into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_nucleo_is_ters_own_and_the_xiaos_are_not() {
        assert_eq!(find("nucleo-f401re").unwrap().chip, "STM32F401RE");
        assert!(find("xiao-esp32c3").is_none());
    }

    #[test]
    fn the_nucleo_project_targets_the_m4f_and_flashes_with_probe_rs() {
        let files = NUCLEO_F401RE.files("blinky");
        let file = |p: &str| {
            &files
                .iter()
                .find(|(path, _)| path == Path::new(p))
                .unwrap_or_else(|| panic!("no {p}"))
                .1
        };
        let cargo: toml::Table = toml::from_str(file("Cargo.toml")).unwrap();
        assert_eq!(cargo["package"]["name"].as_str(), Some("blinky"));
        assert_eq!(
            cargo["dependencies"]["stm32f4xx-hal"]["features"]
                .as_array()
                .unwrap()[0]
                .as_str(),
            Some("stm32f401")
        );
        let config: toml::Table = toml::from_str(file(".cargo/config.toml")).unwrap();
        assert_eq!(
            config["build"]["target"].as_str(),
            Some("thumbv7em-none-eabihf")
        );
        assert_eq!(
            config["target"]["thumbv7em-none-eabihf"]["runner"].as_str(),
            Some("probe-rs run --chip STM32F401RETx")
        );
        let toolchain: toml::Table = toml::from_str(file("rust-toolchain.toml")).unwrap();
        assert_eq!(toolchain["toolchain"]["channel"].as_str(), Some("stable"));
        assert!(file("src/main.rs").contains("pa5"));
        assert!(file("build.rs").contains("memory.x"));
        assert!(files.iter().all(|(_, text)| !text.contains(NAME)));
    }

    #[test]
    fn written_projects_need_an_empty_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut a = args("blinky");
        a.out = Some(tmp.path().join("blinky"));
        let dir = NUCLEO_F401RE.write(&a).unwrap();
        assert!(dir.join(".cargo/config.toml").is_file());
        assert!(dir.join("src/main.rs").is_file());
        let err = NUCLEO_F401RE.write(&a).unwrap_err();
        assert!(err.contains("not empty"), "{err}");
        // An empty folder is fine.
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        a.out = Some(empty);
        NUCLEO_F401RE.write(&a).unwrap();
    }

    #[test]
    fn what_the_template_does_not_offer_is_refused() {
        assert!(NUCLEO_F401RE.check(&args("ok-name_2")).is_ok());
        assert!(NUCLEO_F401RE.check(&args("2fast")).is_err());
        assert!(NUCLEO_F401RE.check(&args("a b")).is_err());
        assert!(NUCLEO_F401RE.check(&args("")).is_err());
        let refused = |f: fn(&mut NewArgs)| {
            let mut a = args("x");
            f(&mut a);
            NUCLEO_F401RE.check(&a).unwrap_err()
        };
        assert!(refused(|a| a.hal = Some("embassy-stm32".into())).contains("stm32f4xx-hal"));
        assert!(refused(|a| a.r#async = true).contains("--async"));
        assert!(refused(|a| a.template = Some("rtic".into())).contains("blinky"));
        assert!(refused(|a| a.logging = Some("defmt".into())).contains("none"));
        assert!(refused(|a| a.flashing = Some("espflash".into())).contains("probe-rs"));
        assert!(refused(|a| a.extra = vec!["--editor".into()]).contains("--"));
        let mut ok = args("x");
        ok.hal = Some("stm32f4xx-hal".into());
        ok.yes = true;
        ok.no_install = true;
        assert!(NUCLEO_F401RE.check(&ok).is_ok());
    }

    #[test]
    fn listings_name_ters_boards() {
        let boards = listing(Listing::Boards, None).unwrap();
        assert!(boards.contains("nucleo-f401re"), "{boards}");
        assert!(boards.contains("STM32F401RE"), "{boards}");
        let hals = listing(Listing::Hals, Some("nucleo-f401re")).unwrap();
        assert_eq!(hals, "  stm32f4xx-hal [default]\n");
        assert!(listing(Listing::Templates, Some("xiao-esp32c3")).is_none());
    }
}
