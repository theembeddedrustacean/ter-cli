//! Where the device token is kept between commands.
//!
//! The system keychain first (Keychain on macOS, Credential Manager on
//! Windows, the Secret Service on Linux and the BSDs). Where none is
//! reachable (a headless machine, WSL, a minimal desktop) the token goes to
//! `token` in the config directory instead, readable only by its owner.
//! `TER_KEYCHAIN=off` skips the keychain and always uses the file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::config_dir;
use crate::output::CliError;

const SERVICE: &str = "ter";
const ACCOUNT: &str = "device-token";
/// The learner's own Wokwi CI token, kept the same way.
const WOKWI_ACCOUNT: &str = "wokwi-token";

/// A place a token can be kept, where the file is not.
pub trait Keychain: Send + Sync {
    /// `Err` when the keychain cannot be reached; the text says why.
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, token: &str) -> Result<(), String>;
    /// `Ok(true)` when there was a token to delete.
    fn delete(&self) -> Result<bool, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Keychain,
    File(PathBuf),
}

impl Location {
    /// For display: `keychain` or `file`.
    pub fn source(&self) -> &'static str {
        match self {
            Location::Keychain => "keychain",
            Location::File(_) => "file",
        }
    }
}

/// Where [`TokenStore::save`] put the token.
pub struct Saved {
    pub location: Location,
    /// Why the keychain was not used, when the token went to the file.
    pub keychain_error: Option<String>,
}

pub struct TokenStore {
    keychain: Option<Box<dyn Keychain>>,
    file: PathBuf,
}

impl TokenStore {
    /// The TER device token's store.
    pub fn open() -> Result<Self, CliError> {
        Self::open_account(ACCOUNT, "token")
    }

    /// The learner's Wokwi token's store: `wokwi-token` in the keychain,
    /// else the `wokwi-token` file.
    pub fn open_wokwi() -> Result<Self, CliError> {
        Self::open_account(WOKWI_ACCOUNT, "wokwi-token")
    }

    /// The learner's key for a hint model provider: `llm-key-<provider>`
    /// in the keychain, else the `llm-key-<provider>` file.
    pub fn open_llm(provider: &str) -> Result<Self, CliError> {
        let name = format!("llm-key-{provider}");
        Self::open_account(name.clone(), &name)
    }

    fn open_account(account: impl Into<String>, file: &str) -> Result<Self, CliError> {
        let account = account.into();
        let keychain_off = std::env::var("TER_KEYCHAIN").is_ok_and(|v| v == "off");
        let keychain: Option<Box<dyn Keychain>> = if keychain_off {
            None
        } else {
            Some(Box::new(SystemKeychain { account }))
        };
        Ok(Self::new(keychain, config_dir()?.join(file)))
    }

    pub fn new(keychain: Option<Box<dyn Keychain>>, file: PathBuf) -> Self {
        Self { keychain, file }
    }

    /// The stored token and where it came from, if there is one.
    pub fn load(&self) -> Result<Option<(String, Location)>, CliError> {
        if let Some(Ok(Some(token))) = self.keychain.as_ref().map(|k| k.get())
            && !token.trim().is_empty()
        {
            return Ok(Some((token.trim().to_string(), Location::Keychain)));
        }
        match std::fs::read_to_string(&self.file) {
            Ok(text) if !text.trim().is_empty() => Ok(Some((
                text.trim().to_string(),
                Location::File(self.file.clone()),
            ))),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_error(format!(
                "Could not read {}: {e}",
                self.file.display()
            ))),
        }
    }

    /// Keep `token`: in the keychain when it can be reached, else in the
    /// file. Only one place holds it afterwards.
    pub fn save(&self, token: &str) -> Result<Saved, CliError> {
        let keychain_error = match self.keychain.as_ref().map(|k| k.set(token)) {
            Some(Ok(())) => {
                remove_file(&self.file)?;
                return Ok(Saved {
                    location: Location::Keychain,
                    keychain_error: None,
                });
            }
            Some(Err(e)) => Some(e),
            None => Some("TER_KEYCHAIN=off".into()),
        };
        write_private(&self.file, token)?;
        Ok(Saved {
            location: Location::File(self.file.clone()),
            keychain_error,
        })
    }

    /// Remove the token from the keychain and the file. Returns where one
    /// was found.
    pub fn clear(&self) -> Result<Vec<Location>, CliError> {
        let mut removed = Vec::new();
        if let Some(Ok(true)) = self.keychain.as_ref().map(|k| k.delete()) {
            removed.push(Location::Keychain);
        }
        if remove_file(&self.file)? {
            removed.push(Location::File(self.file.clone()));
        }
        Ok(removed)
    }
}

fn store_error(message: String) -> CliError {
    CliError::new("token_store_error", message)
}

fn remove_file(path: &Path) -> Result<bool, CliError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(store_error(format!(
            "Could not remove {}: {e}",
            path.display()
        ))),
    }
}

/// Write `token` to `path` so that only the owner can read it: the
/// directory mode 700 when ter creates it, the file mode 600 even when it
/// already existed with a wider mode.
pub(crate) fn write_private(path: &Path, token: &str) -> Result<(), CliError> {
    let fail = |e: std::io::Error| store_error(format!("Could not write {}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(fail)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(fail)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(fail)?;
    }
    writeln!(file, "{token}").map_err(fail)
}

/// The platform's keychain, through `keyring-core`: one entry of the
/// `ter` service.
pub struct SystemKeychain {
    account: String,
}

impl SystemKeychain {
    fn entry(&self) -> Result<keyring_core::Entry, String> {
        static STORE: OnceLock<Result<(), String>> = OnceLock::new();
        STORE.get_or_init(set_default_store).clone()?;
        keyring_core::Entry::new(SERVICE, &self.account).map_err(|e| e.to_string())
    }
}

impl Keychain for SystemKeychain {
    fn get(&self) -> Result<Option<String>, String> {
        match self.entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, token: &str) -> Result<(), String> {
        self.entry()?.set_password(token).map_err(|e| e.to_string())
    }

    fn delete(&self) -> Result<bool, String> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

fn set_default_store() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(any(unix, windows))]
    {
        keyring_core::set_default_store(store.map_err(|e| e.to_string())?);
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    Err("no keychain on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A keychain held in memory, shared with the test so it can look in.
    #[derive(Clone, Default)]
    struct MemoryKeychain(Arc<Mutex<Option<String>>>);

    impl Keychain for MemoryKeychain {
        fn get(&self) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&self, token: &str) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(token.into());
            Ok(())
        }
        fn delete(&self) -> Result<bool, String> {
            Ok(self.0.lock().unwrap().take().is_some())
        }
    }

    /// A machine with no Secret Service.
    struct Unreachable;

    impl Keychain for Unreachable {
        fn get(&self) -> Result<Option<String>, String> {
            Err("no secret service".into())
        }
        fn set(&self, _: &str) -> Result<(), String> {
            Err("no secret service".into())
        }
        fn delete(&self) -> Result<bool, String> {
            Err("no secret service".into())
        }
    }

    fn file_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("ter").join("token")
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn no_keychain_falls_back_to_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(Some(Box::new(Unreachable)), file_in(&dir));

        let saved = store.save("secret").unwrap();
        assert_eq!(saved.location, Location::File(file_in(&dir)));
        assert_eq!(saved.keychain_error.as_deref(), Some("no secret service"));
        #[cfg(unix)]
        {
            assert_eq!(mode(&file_in(&dir)), 0o600);
            assert_eq!(mode(file_in(&dir).parent().unwrap()), 0o700);
        }

        let (token, from) = store.load().unwrap().unwrap();
        assert_eq!(token, "secret");
        assert_eq!(from.source(), "file");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_wider_file_is_narrowed_to_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        TokenStore::new(None, path.clone()).save("new").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "new");
    }

    #[test]
    fn keychain_is_preferred_and_a_stale_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(None, file_in(&dir))
            .save("from-file")
            .unwrap();

        let keychain = MemoryKeychain::default();
        let store = TokenStore::new(Some(Box::new(keychain.clone())), file_in(&dir));
        let saved = store.save("from-keychain").unwrap();
        assert_eq!(saved.location, Location::Keychain);
        assert!(saved.keychain_error.is_none());
        assert!(!file_in(&dir).exists(), "the plain-text copy must go");
        assert_eq!(keychain.0.lock().unwrap().as_deref(), Some("from-keychain"));

        let (token, from) = store.load().unwrap().unwrap();
        assert_eq!(
            (token.as_str(), from),
            ("from-keychain", Location::Keychain)
        );
    }

    #[test]
    fn clear_removes_both() {
        let dir = tempfile::tempdir().unwrap();
        TokenStore::new(None, file_in(&dir))
            .save("in-file")
            .unwrap();
        let keychain = MemoryKeychain::default();
        keychain.set("in-keychain").unwrap();

        let store = TokenStore::new(Some(Box::new(keychain.clone())), file_in(&dir));
        let removed = store.clear().unwrap();
        assert_eq!(
            removed,
            vec![Location::Keychain, Location::File(file_in(&dir))]
        );
        assert!(keychain.0.lock().unwrap().is_none());
        assert!(!file_in(&dir).exists());
        assert!(store.load().unwrap().is_none());
        assert!(
            store.clear().unwrap().is_empty(),
            "a second clear is a no-op"
        );
    }

    #[test]
    fn clear_without_a_keychain_still_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(Some(Box::new(Unreachable)), file_in(&dir));
        store.save("t").unwrap();
        assert_eq!(store.clear().unwrap(), vec![Location::File(file_in(&dir))]);
    }

    #[test]
    fn nothing_stored_loads_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(Some(Box::new(Unreachable)), file_in(&dir));
        assert!(store.load().unwrap().is_none());
        std::fs::create_dir_all(file_in(&dir).parent().unwrap()).unwrap();
        std::fs::write(file_in(&dir), "\n").unwrap();
        assert!(store.load().unwrap().is_none(), "an empty file is no token");
    }
}
