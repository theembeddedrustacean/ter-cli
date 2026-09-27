//! `ter install wokwi-cli`: Wokwi's command line simulator, and the
//! learner's own Wokwi token.
//!
//! TER holds no Wokwi account for anyone: each learner creates a CI token
//! on their own Wokwi account, and ter keeps it like the TER token (the
//! keychain, else a file only they can read). Running it twice is safe:
//! what is already there is left alone.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::output::{CliError, print_json};
use crate::sim::{self, TOKEN_ENV, TOKEN_PAGE, WOKWI_CLI};
use crate::token_store::TokenStore;

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

pub async fn run(tool: &str, token_stdin: bool, json: bool) -> Result<(), CliError> {
    debug_assert_eq!(tool, WOKWI_CLI);
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
        print_json(&Installed {
            tool: WOKWI_CLI,
            path,
            downloaded,
            token: token_source,
            token_stored_now: stored_now,
        });
    } else if stored_now {
        println!("Your Wokwi token is stored ({token_source}). `ter run --sim` is ready.");
    } else {
        println!("Using the Wokwi token from {token_source}. `ter run --sim` is ready.");
    }
    Ok(())
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
    fn this_machine_has_a_release() {
        assert!(asset().unwrap().starts_with("wokwi-cli-"));
    }
}
