//! The Qt-free half of the IDE: what it knows, independent of how it is drawn.
//!
//! Nothing here may import Qt types, so every module below is testable without
//! an event loop.

/// The WSL distribution the daemon runs in unless `settings.json` names
/// another one.
///
/// Here rather than beside the launcher that starts it. It is a default the
/// *configuration* carries, and `Settings::default` reaching into the launcher
/// for it made the settings file depend on the process that reads it -- the
/// wrong way round, and one more edge between two modules that otherwise have
/// nothing to say to each other.
pub const DEFAULT_DISTRO: &str = "bondsymphonic";

pub mod app_state;
pub mod diff;
pub mod editor_buffer;
pub mod file_tree;
pub mod models;
pub mod persistence;
pub mod run_config;
pub mod terminal_grid;
pub mod transcript;
