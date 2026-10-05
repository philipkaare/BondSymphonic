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
    /// The daemon on the other end speaks another version of the wire
    /// protocol. Its own variant rather than an `Rpc` error, because the answer
    /// is not to retry: the two builds are a pair and one of them has to be
    /// replaced. `client` is what this build speaks.
    #[error("daemon speaks protocol {daemon}, this IDE speaks {client}")]
    ProtocolMismatch { daemon: u32, client: u32 },
    #[error("request timed out")]
    Timeout,
}

/// How long a request waits for its response before failing with [`ClientError::Timeout`].
///
/// A heuristic, not a derivation: it is the same thirty seconds the launcher
/// gives the daemon to print its port line, chosen because it is comfortably
/// above the daemon's typical worst case for an ordinary method and short
/// enough that a hung daemon is reported rather than waited on. Nothing on the
/// daemon side guarantees it. Overridable per client with
/// [`DaemonClient::with_request_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What `workspace.merge` gets instead.
///
/// Also a heuristic, sized above the daemon's typical worst case rather than
/// computed from it: the daemon allows itself 60 s per git command
/// (`git::GIT_TIMEOUT`) and a merge is more than one of them, so the 30 s
/// default is a wait shorter than the answer it is waiting for. Two minutes is
/// the round number above that, not a bound the daemon promises -- a merge on a
/// repository large enough to exceed it still ends in a client-side timeout for
/// an operation that is going to succeed.
pub const MERGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// What `workspace.create_pr` gets instead.
///
/// A heuristic again, above the daemon's typical worst case: a pull request is
/// `git push` (60 s) followed by `gh pr create` (120 s, `git::pr::GH_TIMEOUT`),
/// so the daemon is willing to spend three minutes on a call the 30 s default
/// abandons after thirty seconds. Abandoning it is worse
/// than waiting: the push has already happened and the pull request is already
/// being opened, so the user is shown "Pull request failed" for one that
/// succeeded, and the retry that invites answers "a pull request for branch
/// ... already exists".
pub const CREATE_PR_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// What `repo.inspect` and `workspace.create` get instead.
///
/// The same two minutes, for the same kind of reason: both walk a repository
/// the daemon has never touched before, and the daemon allows itself 60 s per
/// git command (`git::GIT_TIMEOUT`) while either call is more than one of
/// them. Measured rather than guessed at: on the first real run an inspect of
/// a large repository reached through `/mnt/c` took longer than the 30 s
/// default, and the New Agent dialog opened with no branches in its list
/// because of it. A create that has to initialise the folder first does that
/// work and more.
///
/// `workspace.restart` gets it too: it starts a sandbox, which is part of what
/// a create does, and may re-register the worktree with git on the way.
///
/// So do `workspace.changes` and `repo.detect_run_configs`, the two the IDE
/// sends against the tree at launch and on every activation. The first is a
/// `git status` plus a walk for untracked files, the second a directory walk,
/// and on an in-place workspace whose checkout is on a Windows drive both run
/// over 9P. Measured from inside the distro on a real repository: the status
/// took over 120 s on the first run after a cold start, 38 s on the next and
/// about 4.5 s once warm. At 30 s both failed on every cold launch, and the
/// Explorer's Changes list opened on `workspace.changes failed: request timed
/// out` for a listing that was going to arrive. Two minutes covers the warm
/// and the second run; the very first after a cold start can still outlast it,
/// and the loading row the list shows meanwhile is what says the wait is a
/// wait and not a failure.
pub const REPOSITORY_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `method` is given when the client has not been told otherwise.
///
/// Keyed on the method name rather than passed in at each call site, so a wait
/// cannot be forgotten at a new caller of an already-slow method: the entries
/// below are the ones whose typical worst case on the daemon outlasts the
/// default. A method this build has never heard of gets the default, which is
/// the right answer for one whose cost nobody here knows.
pub fn default_timeout_for(method: &str) -> Duration {
    match method {
        // A fetch is two git commands under the repository's lock, which a
        // merge of the same repository may be holding.
        "workspace.merge" | "workspace.fetch" => MERGE_REQUEST_TIMEOUT,
        "workspace.create_pr" => CREATE_PR_REQUEST_TIMEOUT,
        "repo.inspect"
        | "workspace.create"
        | "workspace.restart"
        | "workspace.changes"
        | "repo.detect_run_configs" => REPOSITORY_REQUEST_TIMEOUT,
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
                    // A line this build cannot make sense of: an event kind a
                    // newer daemon has and this IDE has not, or one malformed
                    // message. The line has already been consumed, so skipping
                    // it costs that one message; ending the loop here would
                    // instead fail every request in flight with `Disconnected`
                    // and take the session down over a message nobody was
                    // waiting for.
                    Err(ClientError::Proto(e)) => {
                        tracing::warn!("skipping an undecodable line from the daemon: {e}");
                    }
                    // A clean EOF, or the socket itself failing: there is
                    // nothing left to read either way.
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

        let hello: HelloResult = match client
            .request(Request::Hello(HelloParams {
                token: token.into(),
                client_version: client_version.into(),
                protocol_version: Some(PROTOCOL_VERSION),
            }))
            .await
        {
            Ok(hello) => hello,
            // The daemon is the one that spotted the mismatch: it is newer than
            // this IDE and refused the version we sent. Its typed error carries
            // both numbers, so the reply is reported as a mismatch rather than
            // as an opaque `invalid_params`.
            Err(ClientError::Rpc(e)) => {
                return match protocol_mismatch_versions(&e) {
                    Some((daemon, _)) => Err(ClientError::ProtocolMismatch {
                        daemon,
                        client: PROTOCOL_VERSION,
                    }),
                    None => Err(ClientError::Rpc(e)),
                };
            }
            Err(e) => return Err(e),
        };
        // The other direction: the daemon answered happily, on a protocol this
        // IDE does not speak. Stopping at the handshake is the point -- every
        // request after it would fail one field at a time, and none of those
        // failures would say why.
        let daemon = peer_protocol_version(hello.protocol_version);
        if daemon != PROTOCOL_VERSION {
            return Err(ClientError::ProtocolMismatch {
                daemon,
                client: PROTOCOL_VERSION,
            });
        }
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
