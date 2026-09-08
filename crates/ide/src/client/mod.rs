pub mod codec;

use bondsymphonic_proto::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Proto(#[from] bondsymphonic_proto::codec::ProtoError),
    #[error("rpc: {0}")]
    Rpc(RpcError),
    #[error("disconnected")]
    Disconnected,
}

pub type EventStream = mpsc::Receiver<(Option<WorkspaceId>, Event)>;

/// Outcome delivered to a pending request's oneshot. A dedicated `Disconnected` variant
/// (rather than a magic string inside `RpcError`) keeps disconnection detection exact,
/// independent of what a real daemon might legitimately send as an "internal" error.
enum PendingOutcome {
    Value(Value),
    Rpc(RpcError),
    Disconnected,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<PendingOutcome>>>>;

#[derive(Clone)]
pub struct DaemonClient {
    out: mpsc::Sender<ClientMessage>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
}

impl DaemonClient {
    pub async fn connect(
        addr: SocketAddr,
        token: &str,
        client_version: &str,
    ) -> Result<(Self, HelloResult, EventStream), ClientError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);

        let (out_tx, mut out_rx) = mpsc::channel::<ClientMessage>(256);
        let (ev_tx, ev_rx) = mpsc::channel::<(Option<WorkspaceId>, Event)>(1024);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let client = Self {
            out: out_tx,
            pending: pending.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            connected: connected.clone(),
        };

        // Writer task: serializes outgoing requests onto the socket.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if codec::write_message(&mut w, &msg).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: demultiplexes responses (by id) from events (fanned out).
        let conn2 = connected.clone();
        let pend2 = pending.clone();
        tokio::spawn(async move {
            loop {
                match codec::read_message(&mut reader).await {
                    Ok(Some(ServerMessage::Response { id, result, error })) => {
                        if let Some(tx) = pend2.lock().unwrap().remove(&id) {
                            let outcome = match (result, error) {
                                (_, Some(e)) => PendingOutcome::Rpc(e),
                                (Some(v), None) => PendingOutcome::Value(v),
                                (None, None) => PendingOutcome::Value(Value::Null),
                            };
                            let _ = tx.send(outcome);
                        }
                    }
                    Ok(Some(ServerMessage::Event {
                        workspace_id,
                        event,
                    })) => {
                        if ev_tx.send((workspace_id, event)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            conn2.store(false, Ordering::SeqCst);
            // Fail every request still waiting on a response.
            let mut p = pend2.lock().unwrap();
            for (_, tx) in p.drain() {
                let _ = tx.send(PendingOutcome::Disconnected);
            }
        });

        let hello: HelloResult = client
            .request(Request::Hello(HelloParams {
                token: token.into(),
                client_version: client_version.into(),
            }))
            .await?;
        Ok((client, hello, ev_rx))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub async fn request_raw(&self, req: Request) -> Result<Value, ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.out
            .send(ClientMessage::Request { id, request: req })
            .await
            .map_err(|_| ClientError::Disconnected)?;
        match rx.await {
            Ok(PendingOutcome::Value(v)) => Ok(v),
            Ok(PendingOutcome::Rpc(e)) => Err(ClientError::Rpc(e)),
            Ok(PendingOutcome::Disconnected) => Err(ClientError::Disconnected),
            Err(_) => Err(ClientError::Disconnected),
        }
    }

    pub async fn request<T: DeserializeOwned>(&self, req: Request) -> Result<T, ClientError> {
        let v = self.request_raw(req).await?;
        Ok(bondsymphonic_proto::codec::parse_result(v)?)
    }
}
