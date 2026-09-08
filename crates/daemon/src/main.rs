use anyhow::Result;
use bondsymphonic_daemon::sandbox;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
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
    // Task 6 keeps this backend for the workspace registry; for now it decides
    // what the daemon advertises.
    let backend = sandbox::backend_for(backend_name);
    tracing::info!(
        ?data_dir,
        no_sandbox = args.no_sandbox,
        sandbox_backend = backend.name(),
        "starting"
    );

    let server = Server::bind(ServerConfig {
        bind_port: args.port,
        capabilities: bondsymphonic_proto::Capabilities {
            sandbox_backend: backend.name().into(),
            ..ServerConfig::default().capabilities
        },
    })
    .await?;
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
    tracing::info!("daemon exited");
    Ok(())
}
