use crate::client::ClientError;
use bondsymphonic_proto::{codec, ClientMessage, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Read one NDJSON line and decode it as a `ServerMessage`. Blank lines are skipped.
/// Returns `Ok(None)` on clean EOF (peer closed the connection).
pub async fn read_message(
    r: &mut BufReader<OwnedReadHalf>,
) -> Result<Option<ServerMessage>, ClientError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let t = line.trim_end();
        if t.is_empty() {
            continue;
        }
        return Ok(Some(codec::decode(t)?));
    }
}

/// Encode a `ClientMessage` as one NDJSON line and write it.
pub async fn write_message(w: &mut OwnedWriteHalf, msg: &ClientMessage) -> Result<(), ClientError> {
    w.write_all(codec::encode(msg).as_bytes()).await?;
    Ok(())
}
