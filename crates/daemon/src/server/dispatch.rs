use crate::prereqs;
use crate::server::broadcast::EventBus;
use async_trait::async_trait;
use bondsymphonic_proto::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub struct ConnCtx {
    pub authenticated: bool,
    pub events: EventBus,
    pub shutdown: CancellationToken,
}

#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, req: Request, ctx: &mut ConnCtx) -> Result<Value, RpcError>;
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
    async fn handle(&self, req: Request, ctx: &mut ConnCtx) -> Result<Value, RpcError> {
        if let Request::Hello(p) = &req {
            if p.token != self.token {
                return Err(RpcError::unauthorized());
            }
            ctx.authenticated = true;
            return ok(HelloResult {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: self.capabilities.clone(),
            });
        }
        if !ctx.authenticated {
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
