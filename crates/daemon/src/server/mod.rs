pub mod broadcast;
pub mod connection;
pub mod dispatch;
pub mod handlers;

use self::broadcast::EventBus;
use self::dispatch::{Handler, SystemHandler};
use anyhow::Result;
use bondsymphonic_proto::{AgentAdapterKind, Capabilities};
use rand::RngCore;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind_port: u16, // 0 = ephemeral
    pub capabilities: Capabilities,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_port: 0,
            capabilities: Capabilities {
                sandbox_backend: "noop".into(),
                git_protect: false,
                adapters: vec![AgentAdapterKind::Terminal],
            },
        }
    }
}

pub struct Server {
    listener: TcpListener,
    token: String,
    events: EventBus,
    handler: Arc<dyn Handler>,
}

impl Server {
    pub async fn bind(cfg: ServerConfig) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", cfg.bind_port)).await?;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        let events = EventBus::new(1024);
        let handler: Arc<dyn Handler> = Arc::new(SystemHandler {
            token: token.clone(),
            capabilities: cfg.capabilities,
        });
        Ok(Self {
            listener,
            token,
            events,
            handler,
        })
    }
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn event_bus(&self) -> EventBus {
        self.events.clone()
    }
    /// Replace the handler (later milestones wrap SystemHandler with workspace/agent handlers).
    pub fn with_handler(mut self, handler: Arc<dyn Handler>) -> Self {
        self.handler = handler;
        self
    }

    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            // Transient accept errors (e.g. ECONNABORTED, EMFILE) must not
                            // end the accept loop; log, back off briefly, and keep serving.
                            tracing::warn!(error = %e, "accept() failed; retrying");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    tracing::info!(%peer, "client connected");
                    let h = self.handler.clone();
                    let ev = self.events.clone();
                    let sd = shutdown.clone();
                    tokio::spawn(connection::serve_connection(stream, h, ev, sd));
                }
            }
        }
        Ok(())
    }
}
