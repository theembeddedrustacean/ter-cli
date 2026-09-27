//! `ter`'s own settings, in `config.toml` in the platform config directory
//! (`~/.config/ter` on Linux).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::CliError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub site_url: String,
    /// Where `ter ex fetch` puts courses; one folder per course.
    pub courses_root: PathBuf,
    /// Keep this many builds per exercise after a passed run; off when unset.
    pub keep: Option<u32>,
    pub llm_provider: Option<String>,
    pub llm_model: Option<String>,
    pub serve_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        let home = directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_default();
        Self {
            site_url: ter_sdk::DEFAULT_SITE_URL.into(),
            courses_root: home.join("ter-courses"),
            keep: None,
            llm_provider: None,
            llm_model: None,
            serve_port: 7357,
        }
    }
}

/// The config directory: `TER_CONFIG_DIR` when set, else the platform's.
pub fn config_dir() -> Result<PathBuf, CliError> {
    if let Some(dir) = std::env::var_os("TER_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("com", "The Embedded Rustacean", "ter")
        .map(|d| d.config_dir().to_path_buf())
        .ok_or_else(|| CliError::new("config_error", "Could not find a home directory."))
}

impl Config {
    /// Load `config.toml` from the config directory; defaults when it does
    /// not exist. `TER_SITE_URL` overrides `site_url`.
    pub fn load() -> Result<Self, CliError> {
        let mut config = Self::load_from(&config_dir()?.join("config.toml"))?;
        if let Ok(url) = std::env::var("TER_SITE_URL")
            && !url.is_empty()
        {
            config.site_url = url;
        }
        Ok(config)
    }

    fn load_from(path: &Path) -> Result<Self, CliError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                CliError::new(
                    "config_error",
                    format!("{} is not valid: {}", path.display(), e.message()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::new(
                "config_error",
                format!("Could not read {}: {e}", path.display()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load_from(&dir.path().join("config.toml")).unwrap();
        assert_eq!(c.site_url, ter_sdk::DEFAULT_SITE_URL);
        assert_eq!(c.serve_port, 7357);
        assert_eq!(c.keep, None);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "serve_port = 8000\nkeep = 3\n").unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.serve_port, 8000);
        assert_eq!(c.keep, Some(3));
        assert_eq!(c.site_url, ter_sdk::DEFAULT_SITE_URL);
    }

    #[test]
    fn round_trips_through_toml() {
        let c = Config {
            keep: Some(2),
            llm_provider: Some("example".into()),
            ..Config::default()
        };
        let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn unknown_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "site = \"x\"\n").unwrap();
        let err = Config::load_from(&path).unwrap_err();
        assert_eq!(err.code, "config_error");
    }
}
