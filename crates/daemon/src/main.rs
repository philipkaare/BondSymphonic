use anyhow::Result;
use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::sandbox;
use bondsymphonic_daemon::server::dispatch::SystemHandler;
use bondsymphonic_daemon::server::handlers::WorkspaceHandler;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use bondsymphonic_daemon::workspace::DataDirs;
use bondsymphonic_proto::{AgentAdapterKind, Capabilities};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(name = "bondsymphonic-daemon", version)]
struct Args {
    /// Data directory (default ~/.bondsymphonic)
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Run processes without a sandbox (development only)
    #[arg(long)]
    no_sandbox: bool,
    /// trace | debug | info | warn | error
    #[arg(long, default_value = "info")]
    log_level: String,
    /// Bind to a fixed port instead of an ephemeral one (tests/dev)
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Internal: run as the init process inside a sandbox. Not for direct use.
    SandboxInit {
        /// Unix socket to listen on, inside the sandbox.
        #[arg(long)]
        socket: PathBuf,
    },
}

/// Synchronous so `sandbox-init` never starts a tokio runtime: it is PID 1
/// inside the sandbox and runs on blocking std plus threads.
fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        #[cfg(target_os = "linux")]
        Some(Cmd::SandboxInit { ref socket }) => return sandbox::init::run(socket),
        #[cfg(not(target_os = "linux"))]
        Some(Cmd::SandboxInit { .. }) => anyhow::bail!("sandbox-init is Linux only"),
        None => {}
    }
    tokio::runtime::Runtime::new()?.block_on(serve(args))
}

async fn serve(args: Args) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(args.log_level.clone())
        .with_writer(std::io::stderr)
        .init();

    let data_dir = args.data_dir.clone().unwrap_or_else(|| {
        directories::BaseDirs::new()
            .map(|b| b.home_dir().join(".bondsymphonic"))
            .unwrap_or_else(|| PathBuf::from(".bondsymphonic"))
    });
    std::fs::create_dir_all(&data_dir)?;

    let backend_name = if args.no_sandbox || !cfg!(target_os = "linux") {
        "noop"
    } else {
        "linux_bwrap"
    };
    let backend = sandbox::backend_for(backend_name);
    tracing::info!(
        ?data_dir,
        no_sandbox = args.no_sandbox,
        sandbox_backend = backend.name(),
        "starting"
    );
    // `backend_for` downgrades an unsupported name to noop, so the running
    // backend's own name is what gets advertised.
    let capabilities = Capabilities {
        sandbox_backend: backend.name().into(),
        git_protect: backend.name() == "linux_bwrap",
        adapters: vec![AgentAdapterKind::Terminal],
    };

    let server = Server::bind(ServerConfig {
        bind_port: args.port,
        capabilities: capabilities.clone(),
    })
    .await?;
    let daemon = Daemon::new(DataDirs::new(&data_dir), backend, server.event_bus())?;
    daemon.restore().await;
    let system = SystemHandler {
        token: server.token().to_string(),
        capabilities,
    };
    let server = server.with_handler(Arc::new(WorkspaceHandler {
        system,
        daemon: daemon.clone(),
    }));
    // The one and only stdout line: the IDE parses it.
    println!(
        "{}",
        serde_json::json!({ "port": server.port(), "token": server.token() })
    );

    let shutdown = CancellationToken::new();
    // Exit when stdin closes (IDE died) or on ctrl-c.
    let sd = shutdown.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        let mut stdin = tokio::io::stdin();
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        tracing::info!("stdin closed; shutting down");
        sd.cancel();
    });
    let sd = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        sd.cancel();
    });

    server.run(shutdown).await?;
    // Take the handles out under the lock, then shut them down: a `parking_lot`
    // guard must never be held across an await.
    let sandboxes: Vec<_> = daemon.sandboxes.lock().drain().map(|(_, h)| h).collect();
    for h in sandboxes {
        let _ = h.shutdown().await;
    }
    tracing::info!("daemon exited");
    Ok(())
}
