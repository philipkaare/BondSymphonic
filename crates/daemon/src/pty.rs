//! Terminal sessions attached to a workspace sandbox.
//!
//! This is a placeholder: Task 7 implements the manager (open/write/resize/close
//! plus the `pty.output` and `pty.exit` events). It exists now so [`crate::daemon::Daemon`]
//! can own one and workspace teardown can already close a workspace's terminals.

use crate::server::broadcast::EventBus;
use bondsymphonic_proto::WorkspaceId;

pub struct PtyManager {
    /// Where `pty.*` events are published. Unused until Task 7 fills the manager in.
    #[allow(dead_code)]
    events: EventBus,
}

impl PtyManager {
    pub fn new(events: EventBus) -> Self {
        Self { events }
    }

    /// Closes every PTY belonging to `ws`. Stub until Task 7.
    pub async fn close_workspace(&self, _ws: &WorkspaceId) {}
}
