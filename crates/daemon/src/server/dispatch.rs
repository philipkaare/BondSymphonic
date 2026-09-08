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
}

impl ConnCtx {
    pub fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::SeqCst)
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
            ctx.authenticated.store(true, Ordering::SeqCst);
            return ok(HelloResult {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: self.capabilities.clone(),
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
