//! User settings, read from `settings.json` in the per-user config directory,
//! plus the Anthropic API key, which is deliberately *not* kept there.
//!
//! The file records only that a key exists ([`Settings::api_key_set`]); the key
//! itself lives in the Windows credential store, reached through the helpers at
//! the bottom of this module. Nothing in this file ever holds the key in a
//! struct, so no `Debug` output, log line or serialised settings file can leak
//! it: every helper fetches it, hands it to one caller, and drops it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// What a new Claude agent starts on.
///
/// `bypassPermissions`, which is the loud end of the range and is chosen anyway
/// because it is the only end that works. Claude Code asks its host before
/// running a tool that needs approval; the daemon claims to be that host and
/// cannot answer, so the CLI refuses instead of asking -- and every quieter
/// mode turns an approval into a refusal the user never sees and cannot
/// overrule. An agent runs in a sandbox, in a worktree of its own, behind a
/// network proxy, and that is what makes this defensible in the meantime.
///
/// This goes back to `manual` when the CLI can reach the window to ask. See
/// `docs/superpowers/plans/notes/2026-09-13-permission-hang-finding.md`.
const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

/// "Ask before every tool", as the CLI's help spells it.
const DOCUMENTED_ASK_MODE: &str = "manual";

/// The other spelling of [`DOCUMENTED_ASK_MODE`]. The CLI takes both and means
/// the same thing by them -- an agent started either way reports
/// `"permissionMode":"default"` itself -- but only one of the two is in its
/// help text, so a settings file holding this one is read as the documented
/// spelling rather than left showing the user a word they cannot look up.
const UNDOCUMENTED_PERMISSION_MODE: &str = "default";

/// Why [`Settings::try_load`] could not answer with the user's settings.
///
/// It exists so a read-modify-write caller can tell "there is no file yet",
/// which is an ordinary first run and reads as the defaults, from "there is a
/// file and I could not read it", which is the one case where writing the
/// defaults back destroys something.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file is there and will not parse. It has been renamed to `backup`
    /// -- `None` only when even that failed -- so nothing can overwrite it.
    #[error("{path} is not readable JSON")]
    Malformed {
        path: PathBuf,
        backup: Option<PathBuf>,
    },
    /// The file is there and could not be read at all: a permission, a
    /// directory in its place, a disk that answered with an error.
    #[error("{path} could not be read: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Renames an unreadable `settings.json` to `settings.json.bad-<timestamp>` and
/// answers with where it went, or `None` when the rename itself failed.
///
/// A timestamp rather than a fixed suffix, and a counter behind it, because the
/// alternative is a second bad file replacing the backup of the first -- which
/// is the same loss this exists to prevent, one step further along.
fn keep_aside(path: &Path) -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let name = path.file_name()?.to_owned();
    for attempt in 0..100 {
        let mut candidate = name.clone();
        candidate.push(format!(".bad-{stamp}"));
        if attempt > 0 {
            candidate.push(format!("-{attempt}"));
        }
        let target = path.with_file_name(&candidate);
        if target.exists() {
            continue;
        }
        return std::fs::rename(path, &target).ok().map(|()| target);
    }
    None
}

/// The palette a settings file that predates the choice reads back as.
pub const DEFAULT_THEME: &str = "system";

fn default_theme() -> String {
    DEFAULT_THEME.to_owned()
}

/// Whether a settings file that predates the switch shows the turn cost and
/// the system lines. It does: they were the only behaviour there was, and an
/// absent field is not an answer of "hide".
pub const DEFAULT_SHOW_AGENT_META: bool = true;

fn default_show_agent_meta() -> bool {
    DEFAULT_SHOW_AGENT_META
}

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
    pub backends: std::collections::BTreeMap<String,crate::model::backends::BackendSettings>,
    pub default_backend: String,
    /// The permission mode a new Claude agent starts on, as the New Agent
    /// dialog's initial combo value.
    #[serde(skip_serializing)]
    pub default_permission_mode: String,
    /// Which palette the application wears: `system` (whatever Windows is
    /// doing, and whatever it changes to at dusk), `light` or `dark`. A word
    /// rather than a bool because "follow the system" is a third state, not a
    /// missing answer.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Whether a transcript shows the small grey lines -- what a turn cost and
    /// how long it took, and what the agent's own startup reported. Useful
    /// while you are learning what an agent costs, and noise for ever after.
    #[serde(default = "default_show_agent_meta")]
    pub show_agent_meta: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            distro: crate::model::DEFAULT_DISTRO.into(),
            daemon_path: "~/.bondsymphonic/bin/bondsymphonic-daemon".into(),
            log_level: "info".into(),
            api_key_set: false,
            backends: crate::model::backends::default_settings(),
            default_backend: "claude".into(),
            default_permission_mode: DEFAULT_PERMISSION_MODE.into(),
            theme: DEFAULT_THEME.into(),
            show_agent_meta: DEFAULT_SHOW_AGENT_META,
        }
    }
}

impl Settings {
    pub fn from_json(raw:&str) -> Result<Self,serde_json::Error> {
        let value:serde_json::Value=serde_json::from_str(raw)?;
        let mut settings:Self=serde_json::from_value(value.clone())?;
        if value.get("backends").and_then(|b|b.get("claude")).is_none() {
            let mode=if settings.default_permission_mode==UNDOCUMENTED_PERMISSION_MODE {DOCUMENTED_ASK_MODE}else{&settings.default_permission_mode};
            settings.backends.insert("claude".into(),crate::model::backends::BackendSettings{enabled:true,default_model:String::new(),default_permission_mode:mode.into()});
        }
        for (id,defaults) in crate::model::backends::default_settings(){settings.backends.entry(id).or_insert(defaults);}
        let claude=settings.backends.get_mut("claude").expect("Claude defaults inserted");
        if claude.default_permission_mode==UNDOCUMENTED_PERMISSION_MODE {claude.default_permission_mode=DOCUMENTED_ASK_MODE.into();}
        settings.default_permission_mode=claude.default_permission_mode.clone();
        Ok(settings)
    }

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

    /// The settings on disk, or the reason they could not be read.
    ///
    /// No file is not a reason: a first run has none and reads as the defaults.
    /// A file that is there and will not parse *is* one, and it is moved aside
    /// as `settings.json.bad-<timestamp>` before this returns, so the user's
    /// own file survives whatever the caller does next.
    pub fn try_load() -> Result<Self, SettingsError> {
        migrate_legacy_settings();
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(SettingsError::Unreadable { path, source }),
        };
        // Bytes that are not UTF-8 are corruption like any other and get the
        // same move-aside; `read_to_string` would call them an I/O error and
        // leave the file in place.
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|raw| Self::from_json(raw).map_err(|e| e.to_string()));
        match parsed {
            Ok(mut settings) => {
                if settings.default_permission_mode == UNDOCUMENTED_PERMISSION_MODE {
                    // To `manual`, which is what the word meant, and NOT to
                    // [`DEFAULT_PERMISSION_MODE`]: that is `bypassPermissions`
                    // now, and rewriting a stored "ask me" into "run everything
                    // unasked" would be this migration escalating a user who
                    // never agreed to it. Only a fresh install gets the floor.
                    settings.default_permission_mode = DOCUMENTED_ASK_MODE.to_owned();
                }
                Ok(settings)
            }
            Err(detail) => {
                let backup = keep_aside(&path);
                match &backup {
                    Some(to) => tracing::warn!(
                        "{} is not readable ({detail}); kept as {}",
                        path.display(),
                        to.display()
                    ),
                    None => tracing::warn!(
                        "{} is not readable ({detail}) and could not be moved aside",
                        path.display()
                    ),
                }
                Err(SettingsError::Malformed { path, backup })
            }
        }
    }

    /// The settings on disk, falling back to the defaults for anything that
    /// went wrong. For the readers that have nothing to write back: a caller
    /// that saves afterwards must use [`Settings::try_load`] instead, or it
    /// writes the defaults over a file it never managed to read.
    pub fn load() -> Self {
        Self::try_load().unwrap_or_else(|e| {
            tracing::warn!("{e}; using the default settings");
            Self::default()
        })
    }

    /// Writes the settings the way `state.json` is written: a unique temporary
    /// beside the file, flushed to the device, then a rename. A crash or a
    /// power cut mid-write leaves either the previous settings or the new ones,
    /// never a truncated file that the next start has to move aside.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = p.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        // Retain the legacy Rust setter while writing only the backend shape.
        let mut stored=self.clone();
        if let Some(claude)=stored.backends.get_mut("claude") {claude.default_permission_mode=self.default_permission_mode.clone();}
        let json = serde_json::to_string_pretty(&stored)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = crate::model::persistence::temp_path(&p);
        match crate::model::persistence::write_and_rename(&tmp, &p, json.as_bytes()) {
            Ok(()) => Ok(()),
            Err(e) => {
                // A stray temporary beside the real file reads as a
                // half-written settings file to the next person to look.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Records whether a key is in the credential store, leaving every other
    /// setting as it was on disk. Read-modify-write rather than a whole-object
    /// save so a dialog that only touched the key cannot revert a field some
    /// other part of the IDE has written since.
    pub fn record_api_key_set(present: bool) {
        // `try_load`, not `load`: this writes the object back, so a settings
        // file it could not read must stop it. Answering with the defaults and
        // saving them is how a typo in a hand-edited file used to cost the user
        // every other setting in it.
        let mut settings = match Self::try_load() {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("the API key flag was not recorded: {e}");
                return;
            }
        };
        if settings.api_key_set == present {
            return;
        }
        settings.api_key_set = present;
        if let Err(e) = settings.save() {
            tracing::warn!("settings.json could not be written: {e}");
        }
    }

    /// The daemon binary this build ships with: the copy `package.ps1` put
    /// beside the exe, or the one `scripts\build-daemon.ps1` left in
    /// `target\daemon\`. The order, and the `BS_DAEMON_BINARY` override in
    /// front of it, live in the launcher — it is the module that installs the
    /// binary, and the resolution is tested there.
    pub fn local_daemon_binary() -> Option<PathBuf> {
        crate::launcher::local_daemon_binary()
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
    let Some(key) = normalise_api_key(key) else {
        return false;
    };
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

/// The key as it will be stored: surrounding whitespace removed, and `None`
/// when nothing is left.
///
/// A key pasted out of a browser or a terminal carries a trailing newline often
/// enough that storing it verbatim is how a perfectly good key ends up rejected
/// by the API with nothing in the IDE to suggest why. A field holding nothing
/// but spaces is an empty field, and an empty field means "leave the stored key
/// alone", not "store this".
pub fn normalise_api_key(raw: &str) -> Option<&str> {
    let key = raw.trim();
    (!key.is_empty()).then_some(key)
}

/// The stored key, or `None` when there is none. The only reader is
/// `agent.start`'s option builder, which puts it straight into the request.
pub fn api_key() -> Option<String> {
    match entry().and_then(|e| e.get_password()) {
        // Trimmed on the way out too, so a key an earlier build stored with a
        // trailing newline starts working rather than needing to be re-entered.
        Ok(key) => normalise_api_key(&key).map(str::to_owned),
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

pub fn backend_api_key(kind:bondsymphonic_proto::AgentAdapterKind) -> Option<String> {
    if kind==bondsymphonic_proto::AgentAdapterKind::Claude {return api_key();}
    let name=crate::model::backends::credential_name(kind)?;
    match keyring::Entry::new(KEYRING_SERVICE,name).and_then(|e|e.get_password()) {
        Ok(key)=>normalise_api_key(&key).map(str::to_owned),
        Err(keyring::Error::NoEntry)=>None,
        Err(e)=>{tracing::warn!("backend credential could not be read: {e}");None}
    }
}

pub fn set_backend_api_key(kind:bondsymphonic_proto::AgentAdapterKind,key:&str) -> bool {
    if kind==bondsymphonic_proto::AgentAdapterKind::Claude {return set_api_key(key);}
    let Some(name)=crate::model::backends::credential_name(kind) else{return false};
    let Some(key)=normalise_api_key(key) else{return false};
    match keyring::Entry::new(KEYRING_SERVICE,name).and_then(|e|e.set_password(key)) {
        Ok(())=>true,Err(e)=>{tracing::warn!("backend credential could not be stored: {e}");false}
    }
}

pub fn clear_backend_api_key(kind:bondsymphonic_proto::AgentAdapterKind) -> bool {
    if kind==bondsymphonic_proto::AgentAdapterKind::Claude {return clear_api_key();}
    let Some(name)=crate::model::backends::credential_name(kind) else{return false};
    match keyring::Entry::new(KEYRING_SERVICE,name).and_then(|e|e.delete_credential()) {
        Ok(())|Err(keyring::Error::NoEntry)=>true,Err(e)=>{tracing::warn!("backend credential could not be removed: {e}");false}
    }
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
