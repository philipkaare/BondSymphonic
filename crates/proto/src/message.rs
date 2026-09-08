use crate::error::RpcError;
use crate::event::Event;
use crate::ids::WorkspaceId;
use crate::request::Request;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Request {
        id: u64,
        #[serde(flatten)]
        request: Request,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Event {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
        event: Event,
    },
}

impl ServerMessage {
    pub fn ok<T: Serialize>(id: u64, result: &T) -> Self {
        Self::Response {
            id,
            result: Some(serde_json::to_value(result).expect("serializable result")),
            error: None,
        }
    }
    pub fn err(id: u64, error: RpcError) -> Self {
        Self::Response {
            id,
            result: None,
            error: Some(error),
        }
    }
    pub fn event(workspace_id: Option<WorkspaceId>, event: Event) -> Self {
        Self::Event {
            workspace_id,
            event,
        }
    }
}
