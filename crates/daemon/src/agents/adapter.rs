//! Process lifecycle shared by agent adapters.
use async_trait::async_trait;
use bondsymphonic_proto::{PermissionDecision, RpcError};
use serde_json::Value;

#[async_trait]
pub trait AgentAdapter: Send {
    async fn start(&mut self) -> Result<(), RpcError>;
    async fn send(&mut self, text: String) -> Result<(), RpcError>;
    async fn permission_reply(
        &mut self,
        request_id: String,
        decision: PermissionDecision,
        updated_input: Option<Value>,
        message: Option<String>,
    ) -> Result<(), RpcError>;
    async fn interrupt(&mut self) -> Result<(), RpcError>;
    async fn stop(&mut self) -> Result<(), RpcError>;
}
