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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_and_codes_are_distinct() {
        let all = [
            ConnectionState::Disconnected,
            ConnectionState::Launching,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
            ConnectionState::Error,
        ];
        let codes: std::collections::HashSet<i32> = all.iter().map(|s| s.as_i32()).collect();
        assert_eq!(codes.len(), all.len());
        assert_eq!(ConnectionState::Connected.label(), "daemon: connected");
    }
}
