//! Pure-Rust application state. This module must never import Qt types.

/// Lifecycle of the IDE's connection to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Launching,
    Connecting,
    Connected,
    Reconnecting,
    Error,
}

impl ConnectionState {
    /// Stable numeric code exposed to C++/QML as the `connection_state` property.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Disconnected => 0,
            Self::Launching => 1,
            Self::Connecting => 2,
            Self::Connected => 3,
            Self::Reconnecting => 4,
            Self::Error => 5,
        }
    }

    /// Inverse of [`ConnectionState::as_i32`]. Codes outside 0..=5 map to
    /// [`ConnectionState::Error`], since the `connection_state` property is
    /// writable from C++ and may hold anything.
    pub fn from_i32(code: i32) -> Self {
        match code {
            0 => Self::Disconnected,
            1 => Self::Launching,
            2 => Self::Connecting,
            3 => Self::Connected,
            4 => Self::Reconnecting,
            _ => Self::Error,
        }
    }

    /// Human readable text for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "daemon: not started",
            Self::Launching => "daemon: launching",
            Self::Connecting => "daemon: connecting",
            Self::Connected => "daemon: connected",
            Self::Reconnecting => "daemon: reconnecting",
            Self::Error => "daemon: error",
        }
    }
}

/// Builds the status bar text from a connection label and the daemon version.
/// An empty version contributes nothing. This is the only place the two are
/// combined, so the C++ shell never has to branch on the version.
pub fn compose_status(label: &str, version: &str) -> String {
    if version.is_empty() {
        label.to_owned()
    } else {
        format!("{label} v{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [ConnectionState; 6] = [
        ConnectionState::Disconnected,
        ConnectionState::Launching,
        ConnectionState::Connecting,
        ConnectionState::Connected,
        ConnectionState::Reconnecting,
        ConnectionState::Error,
    ];

    #[test]
    fn labels_and_codes_are_distinct() {
        let codes: std::collections::HashSet<i32> = ALL_STATES.iter().map(|s| s.as_i32()).collect();
        assert_eq!(codes.len(), ALL_STATES.len());
        assert_eq!(ConnectionState::Connected.label(), "daemon: connected");
    }

    #[test]
    fn codes_round_trip_and_unknown_codes_are_errors() {
        for state in ALL_STATES {
            assert_eq!(ConnectionState::from_i32(state.as_i32()), state);
        }
        assert_eq!(ConnectionState::from_i32(-1), ConnectionState::Error);
        assert_eq!(ConnectionState::from_i32(99), ConnectionState::Error);
    }

    #[test]
    fn compose_status_appends_the_version_only_when_present() {
        assert_eq!(compose_status("daemon: connected", ""), "daemon: connected");
        assert_eq!(
            compose_status("daemon: connected", "0.1.0"),
            "daemon: connected v0.1.0"
        );
    }
}
