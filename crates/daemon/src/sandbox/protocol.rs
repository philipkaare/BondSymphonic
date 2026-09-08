//! Line-delimited JSON protocol spoken between the daemon's exec client and the
//! in-sandbox init process. One message per line, in both directions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InitRequest {
    Spawn {
        id: u64,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
        pty: Option<(u16, u16)>,
    },
    Kill {
        pid: u32,
        signal: i32,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum InitReply {
    /// Followed on the wire by SCM_RIGHTS fds: `[stdin, stdout, stderr]` when
    /// `has_pty` is false, `[pty_master]` when true.
    Spawned {
        id: u64,
        pid: u32,
        has_pty: bool,
    },
    SpawnFailed {
        id: u64,
        message: String,
    },
    Exited {
        pid: u32,
        code: i32,
    },
    ShuttingDown,
}

/// Serializes a protocol message as one newline-terminated JSON line.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    let mut v = serde_json::to_vec(m).expect("protocol serializes");
    v.push(b'\n');
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_one_json_line() {
        let bytes = encode(&InitRequest::Shutdown);
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(&bytes[..bytes.len() - 1], br#"{"op":"shutdown"}"#);
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let req = InitRequest::Spawn {
            id: 7,
            argv: vec!["sh".into(), "-c".into(), "true".into()],
            env: vec![("A".into(), "b".into())],
            cwd: Some("/work".into()),
            pty: Some((80, 24)),
        };
        let line = encode(&req);
        let back: InitRequest = serde_json::from_slice(&line).unwrap();
        assert!(matches!(
            back,
            InitRequest::Spawn {
                id: 7,
                pty: Some((80, 24)),
                ..
            }
        ));

        let reply = InitReply::Spawned {
            id: 7,
            pid: 42,
            has_pty: true,
        };
        let back: InitReply = serde_json::from_slice(&encode(&reply)).unwrap();
        assert!(matches!(
            back,
            InitReply::Spawned {
                id: 7,
                pid: 42,
                has_pty: true
            }
        ));

        let back: InitReply =
            serde_json::from_slice(&encode(&InitReply::Exited { pid: 42, code: 3 })).unwrap();
        assert!(matches!(back, InitReply::Exited { pid: 42, code: 3 }));
    }
}
