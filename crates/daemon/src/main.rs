use anyhow::Result;
use bondsymphonic_daemon::daemon::Daemon;
use bondsymphonic_daemon::net;
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
    /// Internal: forward the sandbox's proxy port to the daemon's socket. Not
    /// for direct use.
    ProxyShim {
        /// The workspace's proxy socket, as seen from inside the sandbox.
        #[arg(long)]
        socket: PathBuf,
        /// Address to listen on inside the sandbox.
        #[arg(long, default_value = "127.0.0.1:3128")]
        listen: String,
    },
    /// Internal: forward a socket in the sandbox's run directory to a port on
    /// the sandbox's loopback, so the host can reach a run. Not for direct use.
    Forward {
        /// The run's forwarder socket, as seen from inside the sandbox.
        #[arg(long)]
        socket: PathBuf,
        /// The port the run listens on inside the sandbox.
        #[arg(long)]
        port: u16,
    },
}

/// Synchronous so `sandbox-init` never starts a tokio runtime: it is PID 1
/// inside the sandbox and runs on blocking std plus threads. The proxy shim and
/// the port forwarder are ordinary async programs, so each builds a runtime of
/// its own.
fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        #[cfg(target_os = "linux")]
        Some(Cmd::SandboxInit { ref socket }) => return sandbox::init::run(socket),
        #[cfg(not(target_os = "linux"))]
        Some(Cmd::SandboxInit { .. }) => anyhow::bail!("sandbox-init is Linux only"),
        Some(Cmd::ProxyShim {
            ref socket,
            ref listen,
        }) => {
            return tokio::runtime::Runtime::new()?.block_on(net::shim::run(socket, listen));
        }
        Some(Cmd::Forward { ref socket, port }) => {
            return tokio::runtime::Runtime::new()?.block_on(net::forward::run(socket, port));
        }
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
        adapters: vec![AgentAdapterKind::Claude, AgentAdapterKind::Terminal],
    };

    let server = Server::bind(ServerConfig {
        bind_port: args.port,
        capabilities: capabilities.clone(),
    })
    .await?;
    let daemon = Daemon::new(DataDirs::new(&data_dir), backend, server.event_bus())?;
    let system = SystemHandler {
        token: server.token().to_string(),
        capabilities,
    };
    let server = server.with_handler(Arc::new(WorkspaceHandler {
        system,
        daemon: daemon.clone(),
    }));
    // The one and only stdout line: the IDE parses it. It goes out before `restore`,
    // which starts a sandbox per registered workspace and can take seconds; the IDE
    // should not wait on that to learn the port.
    println!(
        "{}",
        serde_json::json!({ "port": server.port(), "token": server.token() })
    );
    // The agents come back before the first connection is accepted. It is one file
    // read, and it has to be finished before a client can call `agent.start`: the
    // ids restore puts in the map are what a new agent's id is minted against.
    daemon.restore_agents();
    // The workspaces run alongside the accept loop rather than ahead of it, so
    // `hello` is answered at once and each workspace flips to Ready or SandboxDown
    // through the `workspace.state` events it publishes as its sandboxes come up.
    let restoring = daemon.clone();
    tokio::spawn(async move { restoring.restore_workspaces().await });

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
    // A setup terminal is an unsandboxed login or install waiting on a person,
    // so it must not outlive the daemon that opened it.
    daemon.shutdown_host().await;
    // Take the handles out under the lock, then shut them down: a `parking_lot`
    // guard must never be held across an await.
    let sandboxes: Vec<_> = daemon.sandboxes.lock().drain().map(|(_, h)| h).collect();
    for h in sandboxes {
        let _ = h.shutdown().await;
    }
    tracing::info!("daemon exited");
    Ok(())
}
