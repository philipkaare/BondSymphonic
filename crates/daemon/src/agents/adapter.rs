//! Process lifecycle shared by agent adapters.
use async_trait::async_trait;
use bondsymphonic_proto::{PermissionDecision, QueuedPromptAction, RpcError};
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
    /// Acts on a message the adapter queued behind the turn in flight. Only an
    /// adapter that queues messages itself has any: Claude's CLI queues its
    /// own input and Codex steers it into the running turn.
    async fn queued_prompt(
        &mut self,
        _prompt_id: String,
        _action: QueuedPromptAction,
    ) -> Result<(), RpcError> {
        Err(RpcError::invalid_params(
            "this agent has no queued messages",
        ))
    }
    async fn stop(&mut self) -> Result<(), RpcError>;
}
