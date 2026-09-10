pub mod codec;
pub mod router;

use bondsymphonic_proto::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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
    #[error("request timed out")]
    Timeout,
}

/// How long a request waits for its response before failing with [`ClientError::Timeout`].
/// Matches the launcher's port-line timeout. Overridable per client with
/// [`DaemonClient::with_request_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What `workspace.merge` gets instead.
///
/// The daemon allows itself 60 s per git command (`git::GIT_TIMEOUT`) and a merge
/// is more than one of them, so 30 s is a wait shorter than the answer it is
/// waiting for.
pub const MERGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// What `workspace.create_pr` gets instead.
///
/// A pull request is `git push` (60 s) followed by `gh pr create` (120 s,
/// `git::pr::GH_TIMEOUT`), so the daemon is willing to spend three minutes on a
/// call the 30 s default abandons after thirty seconds. Abandoning it is worse
/// than waiting: the push has already happened and the pull request is already
/// being opened, so the user is shown "Pull request failed" for one that
/// succeeded, and the retry that invites answers "a pull request for branch
/// ... already exists".
pub const CREATE_PR_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// How long `method` is given when the client has not been told otherwise.
///
/// Keyed on the method name rather than passed in at each call site, so a wait
/// cannot be forgotten at a new caller of an already-slow method: the two
/// entries below are the two the daemon's own budget outlasts the default. A
/// method this build has never heard of gets the default, which is the right
/// answer for one whose cost nobody here knows.
pub fn default_timeout_for(method: &str) -> Duration {
    match method {
        "workspace.merge" => MERGE_REQUEST_TIMEOUT,
        "workspace.create_pr" => CREATE_PR_REQUEST_TIMEOUT,
        _ => DEFAULT_REQUEST_TIMEOUT,
    }
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

/// Owns the reader task's `JoinHandle` and aborts it on drop. Held behind an `Arc` shared
/// by every `DaemonClient` clone so the reader task (and, with it, the socket's read half)
/// is torn down deterministically once the last clone goes away, rather than lingering
/// until the peer notices the write half closed and closes its own side.
struct ReaderGuard(Option<JoinHandle<()>>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

#[derive(Clone)]
pub struct DaemonClient {
    out: mpsc::Sender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
    /// One wait for every method, or `None` to take each method's own from
    /// [`default_timeout_for`]. `None` is what an ordinary client runs with;
    /// `Some` is [`DaemonClient::with_request_timeout`], and it wins outright
    /// so a caller that has to bound a slow method can.
    request_timeout: Option<Duration>,
    _reader_guard: Arc<ReaderGuard>,
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

        // The channel carries *encoded* lines rather than `ClientMessage`s, so
        // one hand-built line with a method the typed enum does not carry can
        // travel the same socket in the same order as everything else. See
        // [`DaemonClient::send_untyped`].
        let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
        let (ev_tx, ev_rx) = mpsc::channel::<(Option<WorkspaceId>, Event)>(1024);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));

        // Writer task: serializes outgoing requests onto the socket. Ends (dropping its
        // write half) once every `DaemonClient` clone is gone and `out_tx` is dropped.
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if codec::write_line(&mut w, &line).await.is_err() {
                    break;
                }
            }
        });

        // Reader task: demultiplexes responses (by id) from events (fanned out). Its
        // handle is owned by `ReaderGuard` below so it is aborted (and its read half
        // dropped) as soon as the last `DaemonClient` clone goes away, rather than
        // lingering until the peer notices and closes its side.
        let conn2 = connected.clone();
        let pend2 = pending.clone();
        let reader_handle: JoinHandle<()> = tokio::spawn(async move {
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

        let client = Self {
            out: out_tx,
            pending: pending.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            connected: connected.clone(),
            request_timeout: None,
            _reader_guard: Arc::new(ReaderGuard(Some(reader_handle))),
        };

        let hello: HelloResult = client
            .request(Request::Hello(HelloParams {
                token: token.into(),
                client_version: client_version.into(),
            }))
            .await?;
        Ok((client, hello, ev_rx))
    }

    /// Sets how long each request waits for its response before failing with
    /// [`ClientError::Timeout`], for every method alike. The setting is per-clone,
    /// so apply it to the client that will issue the requests.
    ///
    /// Without it each method gets [`default_timeout_for`], which is the 30 s
    /// default for everything but the merge and the pull request. An explicit
    /// wait overrides that rather than raising it: the callers that set one are
    /// bounding a request they must not sit on, and a two-minute floor would
    /// take that away from them.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// The wait this client gives `req`: its own if it was given one, and the
    /// method's otherwise.
    fn timeout_for(&self, req: &Request) -> Duration {
        self.request_timeout
            .unwrap_or_else(|| default_timeout_for(req.method_name()))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub async fn request_raw(&self, req: Request) -> Result<Value, ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected);
        }
        let timeout = self.timeout_for(&req);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // The reader task clears `connected` *before* draining the pending map, so an entry
        // inserted after that drain would never be completed by anyone. Re-checking here,
        // after the insert, closes that window: either the drain sees our entry, or we see
        // the flag already cleared and withdraw it ourselves.
        if !self.is_connected() {
            self.pending.lock().unwrap().remove(&id);
            return Err(ClientError::Disconnected);
        }
        if self
            .out
            .send(codec::encode_message(&ClientMessage::Request {
                id,
                request: req,
            }))
            .await
            .is_err()
        {
            // The writer task is gone; nothing will ever complete this oneshot, so
            // remove it instead of leaking it in the pending map.
            self.pending.lock().unwrap().remove(&id);
            return Err(ClientError::Disconnected);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(PendingOutcome::Value(v))) => Ok(v),
            Ok(Ok(PendingOutcome::Rpc(e))) => Err(ClientError::Rpc(e)),
            Ok(Ok(PendingOutcome::Disconnected)) | Ok(Err(_)) => Err(ClientError::Disconnected),
            Err(_) => {
                // A daemon that accepts a request and never answers it must not wedge the
                // caller (nor leak its slot in the pending map) forever.
                self.pending.lock().unwrap().remove(&id);
                Err(ClientError::Timeout)
            }
        }
    }

    pub async fn request<T: DeserializeOwned>(&self, req: Request) -> Result<T, ClientError> {
        let v = self.request_raw(req).await?;
        Ok(bondsymphonic_proto::codec::parse_result(v)?)
    }

    /// **Test-only.** Sends a request whose method the [`Request`] enum does
    /// not carry, and does not wait for an answer.
    ///
    /// It exists for exactly one caller: the smoke script's `reconnect` step,
    /// which asks an in-process *fake* daemon to drop the connection with
    /// `system.test_drop`. A method the real daemon does not have is an unknown
    /// enum variant to its decoder, so it answers `invalid_params` and stays
    /// connected: nothing this sends can make a real daemon do anything it
    /// would not do for a typo.
    ///
    /// Not waiting is the point rather than a shortcut: the method this is for
    /// is answered by the socket closing, so there is no reply to wait for and
    /// no pending entry to leak. Any method that *does* have an answer belongs
    /// in `Request`, where it is typed.
    pub async fn send_untyped(&self, method: &str) -> Result<(), ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params =
            serde_json::json!({ "type": "request", "id": id, "method": method, "params": {} });
        let line = format!("{params}\n");
        self.out
            .send(line)
            .await
            .map_err(|_| ClientError::Disconnected)
    }
}
