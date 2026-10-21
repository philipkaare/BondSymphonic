use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Unauthorized,
    InvalidParams,
    NotFound,
    GitError,
    SandboxError,
    PrereqMissing,
    AgentError,
    IoError,
    Conflict,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
    pub fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "invalid or missing token")
    }
    pub fn invalid_params(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, m)
    }
    pub fn io(e: &std::io::Error) -> Self {
        Self::new(ErrorCode::IoError, e.to_string())
    }

    /// The daemon's answer to a `hello` from a client speaking another version
    /// of the protocol.
    ///
    /// Both numbers travel in `data` beside a fixed reason, so the client can
    /// name them in its status bar without parsing the message, and the
    /// message says the same thing for anyone reading a log.
    pub fn protocol_mismatch(daemon: u32, client: u32) -> Self {
        Self::new(
            ErrorCode::InvalidParams,
            format!("protocol version {client} is not supported (daemon speaks {daemon})"),
        )
        .with_data(serde_json::json!({
            "reason": PROTOCOL_MISMATCH_REASON,
            "daemon": daemon,
            "client": client,
        }))
    }

    /// The answer to `agent.queued_prompt` for a message that is no longer
    /// queued: the worker sent it, or it was discarded, just before the
    /// request arrived. A race the user lost by a moment, not a failure, so it
    /// is marked for the client to let pass.
    pub fn queued_prompt_settled() -> Self {
        Self::invalid_params("That message was already sent or discarded")
            .with_data(serde_json::json!({ "reason": QUEUED_PROMPT_SETTLED_REASON }))
    }
}

/// The `data.reason` a [`RpcError::queued_prompt_settled`] carries.
pub const QUEUED_PROMPT_SETTLED_REASON: &str = "queued_prompt_settled";

/// Whether `err` is a [`RpcError::queued_prompt_settled`].
pub fn is_queued_prompt_settled(err: &RpcError) -> bool {
    err.data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        == Some(QUEUED_PROMPT_SETTLED_REASON)
}

/// The `data.reason` a [`RpcError::protocol_mismatch`] carries. Written by the
/// daemon, matched by the client.
pub const PROTOCOL_MISMATCH_REASON: &str = "protocol_mismatch";

/// The two versions out of a [`RpcError::protocol_mismatch`], or `None` when
/// `err` is any other error.
pub fn protocol_mismatch_versions(err: &RpcError) -> Option<(u32, u32)> {
    let data = err.data.as_ref()?;
    if data.get("reason")?.as_str()? != PROTOCOL_MISMATCH_REASON {
        return None;
    }
    let daemon = u32::try_from(data.get("daemon")?.as_u64()?).ok()?;
    let client = u32::try_from(data.get("client")?.as_u64()?).ok()?;
    Some((daemon, client))
}
