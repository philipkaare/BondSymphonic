use crate::prereqs;
use crate::server::broadcast::EventBus;
use async_trait::async_trait;
use bondsymphonic_proto::*;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Per-connection state shared by every request handled on that connection. All fields are
/// shared-by-reference so requests can be dispatched concurrently: handlers take `&ConnCtx`,
/// never `&mut`, and the connection loop keeps its own clone to gate event delivery.
pub struct ConnCtx {
    pub authenticated: Arc<AtomicBool>,
    pub events: EventBus,
    pub shutdown: CancellationToken,
    /// Cancelled when this connection ends, however it ends. A handler that starts
    /// something on the peer's behalf which must not outlive the peer — a host setup
    /// terminal, say — ties it to this.
    pub disconnected: CancellationToken,
    /// The optional events this client asked for in `hello`
    /// (`HelloParams::features`). Empty until then, and for a client that
    /// predates them: such a client is never sent an event it cannot decode.
    pub features: Arc<std::sync::RwLock<Vec<String>>>,
}

impl ConnCtx {
    pub fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::SeqCst)
    }

    /// Whether `msg` goes out on this connection: nothing before `hello`, and
    /// an opt-in event only to a client that asked for it.
    pub fn forwards(&self, msg: &ServerMessage) -> bool {
        if !self.is_authenticated() {
            return false;
        }
        let ServerMessage::Event { event, .. } = msg else {
            return true;
        };
        match event.opt_in_feature() {
            None => true,
            Some(feature) => self
                .features
                .read()
                .map(|f| f.iter().any(|w| w == feature))
                .unwrap_or(false),
        }
    }
}

#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<Value, RpcError>;
}

pub struct SystemHandler {
    pub token: String,
    pub capabilities: Capabilities,
}

fn ok<T: serde::Serialize>(v: T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(e.to_string()))
}

#[async_trait]
impl Handler for SystemHandler {
    async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<Value, RpcError> {
        if let Request::Hello(p) = &req {
            if p.token != self.token {
                return Err(RpcError::unauthorized());
            }
            // The version gate, after the token so an unauthenticated peer
            // learns nothing about this build. A client on another version is
            // refused here rather than left to fail on the first field one of
            // the two has never heard of; the connection loop closes the
            // socket after the reply, because this connection stays
            // unauthenticated.
            let client = peer_protocol_version(p.protocol_version);
            if client != PROTOCOL_VERSION {
                tracing::warn!(
                    daemon = PROTOCOL_VERSION,
                    client,
                    client_version = %p.client_version,
                    "refused a client speaking another protocol version"
                );
                return Err(RpcError::protocol_mismatch(PROTOCOL_VERSION, client));
            }
            if let Ok(mut features) = ctx.features.write() {
                features.clone_from(&p.features);
            }
            ctx.authenticated.store(true, Ordering::SeqCst);
            return ok(HelloResult {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: self.capabilities.clone(),
                protocol_version: Some(PROTOCOL_VERSION),
            });
        }
        if !ctx.is_authenticated() {
            return Err(RpcError::unauthorized());
        }
        match req {
            Request::SystemCheckPrereqs {} => ok(CheckPrereqsResult {
                items: prereqs::check_all().await,
            }),
            Request::SystemShutdown {} => {
                ctx.shutdown.cancel();
                ok(Empty {})
            }
            other => Err(RpcError::internal(format!(
                "not implemented: {}",
                other.method_name()
            ))),
        }
    }
}

/// Test-support handler: delegates to `inner`, optionally sleeping first for requests
/// selected by `delay_for`. Integration tests install it via [`crate::server::Server::with_handler`]
/// to prove that a slow request does not block later requests on the same connection.
#[doc(hidden)]
pub struct DelayingHandler {
    pub inner: Arc<dyn Handler>,
    pub delay_for: fn(&Request) -> Option<std::time::Duration>,
}

#[async_trait]
impl Handler for DelayingHandler {
    async fn handle(&self, req: Request, ctx: &ConnCtx) -> Result<Value, RpcError> {
        if let Some(d) = (self.delay_for)(&req) {
            tokio::time::sleep(d).await;
        }
        self.inner.handle(req, ctx).await
    }
}
