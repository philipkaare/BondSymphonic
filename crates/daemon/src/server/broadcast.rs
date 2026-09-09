use bondsymphonic_proto::{Event, ServerMessage, WorkspaceId};
use tokio::sync::broadcast;

/// Capacity of each connection's event backlog. A client that falls this far behind on
/// unread events (a slow reader, or a burst of `pty.output`) starts dropping the oldest
/// ones rather than growing without bound; the connection loop tells it how many it lost.
pub const EVENT_BUS_CAPACITY: usize = 4096;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<ServerMessage>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity).0,
        }
    }
    pub fn publish(&self, workspace_id: Option<WorkspaceId>, event: Event) {
        // Ignore "no receivers": events before any client connects are simply dropped.
        let _ = self.tx.send(ServerMessage::event(workspace_id, event));
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ServerMessage> {
        self.tx.subscribe()
    }
}
