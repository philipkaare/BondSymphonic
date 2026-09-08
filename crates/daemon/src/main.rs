use anyhow::Result;
use bondsymphonic_daemon::server::{Server, ServerConfig};
use clap::Parser;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
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
    tracing::info!(?data_dir, no_sandbox = args.no_sandbox, "starting");

    let server = Server::bind(ServerConfig {
        bind_port: args.port,
        ..ServerConfig::default()
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
