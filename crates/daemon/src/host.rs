//! The machine the user runs BondSymphonic on, as the IDE that launched this
//! daemon said (`BS_HOST_OS`).
//!
//! The daemon always runs in Linux -- a WSL distribution under Windows, a Lima
//! VM on a Mac -- so nothing it can see says which. It matters wherever the
//! daemon tells a person (or an agent that will tell one) where to go: Settings
//! is File › Settings… on Windows and BondSymphonic › Preferences… on a Mac (Qt
//! moves the item into the app menu and gives it the Mac name), shortcuts
//! use Ctrl on one and ⌘ on the other, and "the WSL distribution" is wrong on
//! a Mac. An IDE that did not say -- an older one, or a test -- gets wording
//! that is true on both.

use std::sync::OnceLock;

/// The variable the IDE's launcher sets on the daemon: `windows`, `macos` or
/// `linux`.
pub const HOST_OS_ENV: &str = "BS_HOST_OS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    MacOs,
    Linux,
    Unknown,
}

/// The host this daemon's IDE runs on, read once.
pub fn os() -> HostOs {
    static OS: OnceLock<HostOs> = OnceLock::new();
    *OS.get_or_init(|| HostOs::parse(std::env::var(HOST_OS_ENV).ok().as_deref()))
}

impl HostOs {
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("windows") => Self::Windows,
            Some("macos") | Some("mac") | Some("darwin") => Self::MacOs,
            Some("linux") => Self::Linux,
            _ => Self::Unknown,
        }
    }

    /// Where Settings is in the IDE, as a path to follow.
    pub fn settings(self) -> &'static str {
        match self {
            Self::Windows | Self::Linux => "File › Settings…",
            Self::MacOs => "BondSymphonic › Preferences… (⌘,)",
            Self::Unknown => "Settings",
        }
    }

    /// The Linux the daemon and its sandboxes run in, as a person knows it.
    pub fn linux_env(self) -> &'static str {
        match self {
            Self::Windows => "the WSL distribution",
            Self::MacOs => "the Lima VM",
            Self::Linux | Self::Unknown => "the daemon's Linux environment",
        }
    }

    /// One sentence for an agent about the machine the user is at, or empty
    /// when it is not known.
    pub fn agent_sentence(self) -> String {
        match self {
            Self::Windows => format!(
                "The user runs the BondSymphonic IDE on Windows (the daemon and this sandbox run \
                 in WSL): when you point them at the IDE, Settings is {} and shortcuts use Ctrl.",
                self.settings()
            ),
            Self::MacOs => format!(
                "The user runs the BondSymphonic IDE on a Mac (the daemon and this sandbox run in \
                 a Lima VM): when you point them at the IDE, Settings is {} and shortcuts use ⌘.",
                self.settings()
            ),
            Self::Linux => format!(
                "The user runs the BondSymphonic IDE on Linux: when you point them at the IDE, \
                 Settings is {}.",
                self.settings()
            ),
            Self::Unknown => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_host_is_pointed_at_its_own_menus() {
        assert_eq!(HostOs::parse(Some("windows")), HostOs::Windows);
        assert_eq!(HostOs::parse(Some(" macOS ")), HostOs::MacOs);
        assert_eq!(HostOs::parse(Some("linux")), HostOs::Linux);
        assert_eq!(HostOs::parse(Some("beos")), HostOs::Unknown);
        assert_eq!(HostOs::parse(None), HostOs::Unknown);
        assert!(HostOs::Windows.settings().starts_with("File"));
        assert!(HostOs::MacOs.settings().contains("BondSymphonic"));
        assert!(HostOs::MacOs.agent_sentence().contains("⌘"));
        assert!(!HostOs::MacOs.agent_sentence().contains("WSL"));
        assert!(HostOs::Windows.agent_sentence().contains("Ctrl"));
        assert!(HostOs::Unknown.agent_sentence().is_empty());
        assert!(!HostOs::Unknown.settings().contains("File"));
    }
}
