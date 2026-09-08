use bondsymphonic_proto::{Event, ServerMessage, WorkspaceId};
use tokio::sync::broadcast;

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
