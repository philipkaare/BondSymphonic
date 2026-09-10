//! User settings, read from `settings.json` in the per-user config directory,
//! plus the Anthropic API key, which is deliberately *not* kept there.
//!
//! The file records only that a key exists ([`Settings::api_key_set`]); the key
//! itself lives in the Windows credential store, reached through the helpers at
//! the bottom of this module. Nothing in this file ever holds the key in a
//! struct, so no `Debug` output, log line or serialised settings file can leak
//! it: every helper fetches it, hands it to one caller, and drops it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Overrides where `settings.json` is read from and written to. **Test-only**:
/// nothing in the shipped IDE sets it, and with it unset [`Settings::path`]
/// answers with the per-user config directory exactly as before. It exists so a
/// test can round-trip the file without touching the developer's own settings.
pub const SETTINGS_PATH_ENV: &str = "BS_SETTINGS_PATH";

/// Overrides where `state.json` is read from and written to, exactly as
/// [`SETTINGS_PATH_ENV`] does for the settings file. **Test-only.** With it
/// unset the state file sits beside `settings.json`.
pub const STATE_PATH_ENV: &str = "BS_STATE_PATH";

/// Overrides the pre-flatten settings location [`Settings::load`] migrates
/// from. **Test-only**: a migration test needs an "old path" it can create
/// without touching the developer's own `%APPDATA%`. With it unset the old
/// path is the `ProjectDirs` one earlier builds wrote to -- unless
/// [`SETTINGS_PATH_ENV`] is set and this is not, which is a test pointing at a
/// temp settings file; that must not pull the developer's real settings into
/// it, so migration is off in that case.
pub const LEGACY_SETTINGS_PATH_ENV: &str = "BS_LEGACY_SETTINGS_PATH";

/// The directory both files live in: `%APPDATA%\BondSymphonic`.
///
/// Flattened from the `ProjectDirs` layout, which nested a second
/// `BondSymphonic` inside the first (IDE spec §11). See [`legacy_path`] for
/// what happens to a file left at the old location.
fn config_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.config_dir().join("BondSymphonic"))
}

/// A non-empty environment override, as a path.
fn path_override(var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(var)?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// Where earlier builds kept `settings.json`, or `None` when there is nothing
/// to migrate from.
fn legacy_path() -> Option<PathBuf> {
    if let Some(over) = path_override(LEGACY_SETTINGS_PATH_ENV) {
        return Some(over);
    }
    // A test pointing `BS_SETTINGS_PATH` at a temp file has no business
    // reading the developer's real settings, so there is no legacy file for it
    // unless it named one itself.
    if path_override(SETTINGS_PATH_ENV).is_some() {
        return None;
    }
    directories::ProjectDirs::from("", "BondSymphonic", "BondSymphonic")
        .map(|d| d.config_dir().join("settings.json"))
}

/// Copies a pre-flatten `settings.json` up to its new home, once.
///
/// "Once" needs no marker file: the copy only happens while there is no file
/// at the new path, and the copy itself creates one. The old file is left
/// where it is rather than deleted, so an older build the user goes back to
/// still finds its settings.
fn migrate_legacy_settings() {
    let Some(new_path) = Settings::path() else {
        return;
    };
    if new_path.exists() {
        return;
    }
    let Some(old_path) = legacy_path() else {
        return;
    };
    if old_path == new_path || !old_path.exists() {
        return;
    }
    if let Some(dir) = new_path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("{} could not be created: {e}", dir.display());
            return;
        }
    }
    match std::fs::copy(&old_path, &new_path) {
        Ok(_) => tracing::info!(
            "settings migrated from {} to {}",
            old_path.display(),
            new_path.display()
        ),
        Err(e) => tracing::warn!(
            "settings could not be migrated from {}: {e}",
            old_path.display()
        ),
    }
}

/// The credential-store service name. One entry, under one user name, for the
/// whole application.
const KEYRING_SERVICE: &str = "BondSymphonic";
const KEYRING_USER: &str = "anthropic_api_key";

/// What `permission_mode` means when the user has never chosen one. Spelled as
/// the Claude CLI spells it, because it is passed through verbatim.
const DEFAULT_PERMISSION_MODE: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub distro: String,
    pub daemon_path: String,
    pub log_level: String,
    /// Whether an Anthropic API key is in the credential store. A hint, not the
    /// authority: the store is asked before the key is used. It is here so the
    /// settings dialog can say "stored in the Windows credential store" without
    /// reading the key back out just to find out whether there is one.
    pub api_key_set: bool,
    /// The permission mode a new Claude agent starts on, as the New Agent
    /// dialog's initial combo value.
    pub default_permission_mode: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            distro: "bondsymphonic".into(),
            daemon_path: "~/.bondsymphonic/bin/bondsymphonic-daemon".into(),
            log_level: "info".into(),
            api_key_set: false,
            default_permission_mode: DEFAULT_PERMISSION_MODE.into(),
        }
    }
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        path_override(SETTINGS_PATH_ENV).or_else(|| config_dir().map(|d| d.join("settings.json")))
    }

    /// Where `state.json` lives: beside `settings.json`, so overriding the
    /// settings path in a test moves both out of the user's `%APPDATA%`
    /// together, and `BS_STATE_PATH` moves the state file on its own.
    pub fn state_path() -> Option<PathBuf> {
        if let Some(over) = path_override(STATE_PATH_ENV) {
            return Some(over);
        }
        Self::path()?
            .parent()
            .map(|dir| dir.join("state.json"))
            .or_else(|| config_dir().map(|d| d.join("state.json")))
    }

    pub fn load() -> Self {
        migrate_legacy_settings();
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, serde_json::to_string_pretty(self).unwrap())
    }

    /// Records whether a key is in the credential store, leaving every other
    /// setting as it was on disk. Read-modify-write rather than a whole-object
    /// save so a dialog that only touched the key cannot revert a field some
    /// other part of the IDE has written since.
    pub fn record_api_key_set(present: bool) {
        let mut settings = Self::load();
        if settings.api_key_set == present {
            return;
        }
        settings.api_key_set = present;
        if let Err(e) = settings.save() {
            tracing::warn!("settings.json could not be written: {e}");
        }
    }

    /// Dev builds: the daemon binary produced by scripts/build-daemon.ps1.
    pub fn local_daemon_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let candidates = [
            // packaged next to the IDE executable
            exe.parent()?.join("bondsymphonic-daemon"),
            // target\<profile>\..\daemon\bondsymphonic-daemon
            exe.parent()?
                .parent()?
                .join("daemon")
                .join("bondsymphonic-daemon"),
        ];
        candidates.into_iter().find(|p| p.exists())
    }
}

/// The one credential-store entry, or the error that opening it produced.
fn entry() -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

/// Stores `key`, replacing whatever was there, and records the fact in
/// `settings.json`. Returns whether it was stored.
///
/// `key` is never logged, and never appears in the error text either: the
/// `keyring` errors carry the *entry*'s identity, not its secret.
pub fn set_api_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    match entry().and_then(|e| e.set_password(key)) {
        Ok(()) => {
            Settings::record_api_key_set(true);
            tracing::info!("Anthropic API key stored in the credential store");
            true
        }
        Err(e) => {
            tracing::warn!("the Anthropic API key could not be stored: {e}");
            false
        }
    }
}

/// The stored key, or `None` when there is none. The only reader is
/// `agent.start`'s option builder, which puts it straight into the request.
pub fn api_key() -> Option<String> {
    match entry().and_then(|e| e.get_password()) {
        Ok(key) if !key.is_empty() => Some(key),
        Ok(_) => None,
        // The ordinary "no key has been stored" answer, not a failure.
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::warn!("the Anthropic API key could not be read: {e}");
            None
        }
    }
}

/// Whether a key is stored. Asks the store rather than trusting the settings
/// flag, so a key deleted in Credential Manager is not still advertised.
pub fn api_key_set() -> bool {
    api_key().is_some()
}

/// Removes the key. Deleting one that is not there is success: the caller asked
/// for there to be no key, and there is none.
pub fn clear_api_key() -> bool {
    let removed = match entry().and_then(|e| e.delete_credential()) {
        Ok(()) => true,
        Err(keyring::Error::NoEntry) => true,
        Err(e) => {
            tracing::warn!("the Anthropic API key could not be removed: {e}");
            false
        }
    };
    if removed {
        Settings::record_api_key_set(false);
        tracing::info!("Anthropic API key removed from the credential store");
    }
    removed
}
