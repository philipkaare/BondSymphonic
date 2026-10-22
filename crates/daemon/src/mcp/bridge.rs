//! The sandbox helper carries bounded JSON lines over one assigned socket.
use super::transport::{error, MAX_BODY};
use bondsymphonic_proto::RpcError;
use std::path::Path;
#[cfg(unix)]
use tokio::io::BufReader;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Retains incomplete lines when another select branch receives a response.
#[derive(Default)]
pub struct FrameBuffer {
    bytes: Vec<u8>,
}
impl FrameBuffer {
    pub async fn read<R: AsyncBufRead + Unpin>(
        &mut self,
        reader: &mut R,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, RpcError> {
        loop {
            let chunk = reader
                .fill_buf()
                .await
                .map_err(|_| error("bridge read failed"))?;
            if chunk.is_empty() {
                return if self.bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(error("incomplete stdio frame"))
                };
            }
            let end = chunk.iter().position(|b| *b == b'\n');
            let count = end.map_or(chunk.len(), |n| n + 1);
            if self.bytes.len() + count > limit {
                return Err(error("stdio frame too large"));
            }
            self.bytes.extend_from_slice(&chunk[..count]);
            reader.consume(count);
            if end.is_some() {
                return Ok(Some(std::mem::take(&mut self.bytes)));
            }
        }
    }
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, RpcError> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .await
            .map_err(|_| error("bridge read failed"))?;
        if chunk.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(error("incomplete stdio frame"))
            };
        }
        let end = chunk.iter().position(|b| *b == b'\n');
        let count = end.map_or(chunk.len(), |n| n + 1);
        if frame.len() + count > limit {
            return Err(error("stdio frame too large"));
        }
        frame.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(frame));
        }
    }
}

pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &serde_json::Value,
) -> Result<(), RpcError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| error("invalid bridge message"))?;
    if bytes.len() >= MAX_BODY {
        return Err(error("stdio frame too large"));
    }
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| error("bridge write failed"))?;
    writer
        .flush()
        .await
        .map_err(|_| error("bridge write failed"))
}

pub async fn copy_frames<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> Result<(), RpcError> {
    while let Some(frame) = read_frame(reader, MAX_BODY).await? {
        let message: serde_json::Value =
            serde_json::from_slice(&frame).map_err(|_| error("invalid stdio JSON"))?;
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            return Err(error("invalid stdio JSON-RPC"));
        }
        writer
            .write_all(&frame)
            .await
            .map_err(|_| error("bridge write failed"))?;
        writer
            .flush()
            .await
            .map_err(|_| error("bridge write failed"))?;
    }
    Ok(())
}

#[cfg(unix)]
pub async fn run(socket: &Path, token: &str) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    write_message(&mut write, &serde_json::json!({"token":token}))
        .await
        .map_err(|_| anyhow::anyhow!("MCP bridge authentication failed"))?;
    let mut remote = BufReader::new(read);
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    tokio::select! {
        r = copy_frames(&mut input, &mut write) => r.map_err(|_| anyhow::anyhow!("MCP bridge input closed")),
        r = copy_frames(&mut remote, &mut output) => r.map_err(|_| anyhow::anyhow!("MCP bridge connection closed")),
    }
}

#[cfg(not(unix))]
pub async fn run(_socket: &Path, _token: &str) -> anyhow::Result<()> {
    anyhow::bail!("MCP sandbox bridge requires Unix sockets")
}
