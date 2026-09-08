use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encode one message as a single NDJSON line (terminated by '\n', no inner newlines).
pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol types always serialize");
    s.push('\n');
    s
}

pub fn decode<T: DeserializeOwned>(line: &str) -> Result<T, ProtoError> {
    Ok(serde_json::from_str(line)?)
}

/// Parse a typed result out of a `ServerMessage::Response.result` value.
pub fn parse_result<T: DeserializeOwned>(v: serde_json::Value) -> Result<T, ProtoError> {
    Ok(serde_json::from_value(v)?)
}
