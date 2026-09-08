//! User settings, read from `settings.json` in the per-user config directory.
//! Plain Rust for now; a QObject wrapper arrives in Milestone 6.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub distro: String,
    pub daemon_path: String,
    pub log_level: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            distro: "bondsymphonic".into(),
            daemon_path: "~/.bondsymphonic/bin/bondsymphonic-daemon".into(),
            log_level: "info".into(),
        }
    }
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "BondSymphonic", "BondSymphonic")
            .map(|d| d.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
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
