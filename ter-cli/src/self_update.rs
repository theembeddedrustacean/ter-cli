//! `ter self-update`.
//!
//! Until release binaries exist this prints the command that reinstalls
//! `ter` from wherever it was installed from, read from cargo's own record
//! of installed crates.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::output::{CliError, print_json};

const PACKAGE: &str = "ter-cli";

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Reinstall {
    source: &'static str,
    command: String,
}

pub fn run(json: bool) -> Result<(), CliError> {
    let record = cargo_home()
        .and_then(|home| std::fs::read_to_string(home.join(".crates2.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let plan = reinstall_command(record.as_ref());

    if json {
        print_json(&plan);
    } else {
        println!("To update ter, run:\n\n    {}\n", plan.command);
    }
    Ok(())
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".cargo")))
}

/// Build the reinstall command from cargo's `.crates2.json`, whose keys
/// look like `ter-cli 0.1.0 (git+https://host/repo.git#<sha>)`.
fn reinstall_command(crates2: Option<&Value>) -> Reinstall {
    let source = crates2
        .and_then(|v| v.get("installs"))
        .and_then(Value::as_object)
        .and_then(|installs| {
            installs.keys().find_map(|key| {
                let rest = key.strip_prefix(PACKAGE)?.strip_prefix(' ')?;
                let (_, source) = rest.split_once(" (")?;
                source.strip_suffix(')').map(str::to_string)
            })
        });

    match source {
        Some(s) if s.starts_with("git+") => {
            let url = s["git+".len()..]
                .split(['?', '#'])
                .next()
                .unwrap_or_default();
            Reinstall {
                source: "git",
                command: format!("cargo install --git {url} {PACKAGE} --force"),
            }
        }
        Some(s) if s.starts_with("path+") => {
            let path = s["path+".len()..].trim_start_matches("file://");
            Reinstall {
                source: "path",
                command: format!("git -C {path} pull && cargo install --path {path} --force"),
            }
        }
        Some(s) if s.starts_with("registry+") || s.starts_with("sparse+") => Reinstall {
            source: "registry",
            command: format!("cargo install {PACKAGE} --force"),
        },
        _ => Reinstall {
            source: "unknown",
            command: format!(
                "cargo install --git <the repository you installed ter from> {PACKAGE} --force"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn installs(key: &str) -> Value {
        json!({"installs": {
            "ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)": {},
            key: {"bins": ["ter"]}
        }})
    }

    #[test]
    fn git_install_reinstalls_from_the_same_repository() {
        let v = installs(
            "ter-cli 0.1.0 (git+https://example.com/org/ter-cli.git?branch=main#0123abcd)",
        );
        let r = reinstall_command(Some(&v));
        assert_eq!(r.source, "git");
        assert_eq!(
            r.command,
            "cargo install --git https://example.com/org/ter-cli.git ter-cli --force"
        );
    }

    #[test]
    fn registry_install_reinstalls_from_the_registry() {
        let v = installs("ter-cli 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)");
        assert_eq!(
            reinstall_command(Some(&v)).command,
            "cargo install ter-cli --force"
        );
    }

    #[test]
    fn path_install_rebuilds_the_checkout() {
        let v = installs("ter-cli 0.1.0 (path+file:///home/me/src/ter-cli/ter-cli)");
        let r = reinstall_command(Some(&v));
        assert_eq!(r.source, "path");
        assert!(r.command.contains("--path /home/me/src/ter-cli/ter-cli"));
    }

    #[test]
    fn no_record_gives_a_generic_line() {
        assert_eq!(reinstall_command(None).source, "unknown");
        let v = installs("ter-cli-other 1.0.0 (registry+x)");
        assert_eq!(reinstall_command(Some(&v)).source, "unknown");
    }
}
