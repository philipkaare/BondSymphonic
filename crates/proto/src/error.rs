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
}
