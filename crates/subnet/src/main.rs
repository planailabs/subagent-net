use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use subnet::hub::{Hub, http};
use subnet::spawner::{Spawner, attach, config::Config};

#[derive(Parser)]
#[command(version, about = "Distributed network of resumable LLM agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct HubArgs {
    #[arg(long, env = "DATABASE_URL")]
    db: String,
    #[arg(long, env = "SUBNET_LISTEN", default_value = "127.0.0.1:7700")]
    listen: String,
    /// Shared secret spawners and clients must present.
    #[arg(long, env = "SUBNET_TOKEN")]
    token: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the hub.
    Hub(HubArgs),
    /// Run a spawner that connects to the hub named in its config.
    Spawner {
        #[arg(long, short)]
        config: PathBuf,
    },
    /// Run a hub and a spawner in one process.
    Dev {
        #[command(flatten)]
        hub: HubArgs,
        #[arg(long, short)]
        config: PathBuf,
    },
}

async fn serve_hub(a: HubArgs) -> anyhow::Result<Arc<Hub>> {
    let hub = Hub::open(&a.db, a.token).await?;
    let l = tokio::net::TcpListener::bind(&a.listen).await?;
    tracing::info!(listen = %a.listen, "hub listening");
    let router = http::router(hub.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(l, router).await {
            tracing::error!(error = %e, "hub server stopped");
            std::process::exit(1);
        }
    });
    Ok(hub)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match Cli::parse().cmd {
        Cmd::Hub(a) => {
            serve_hub(a).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Spawner { config } => {
            let cfg = Config::load(&config)?;
            let spawner = Arc::new(Spawner::new(&cfg).await?);
            subnet::spawner::ws::run(spawner, &cfg.hub).await?;
        }
        Cmd::Dev { hub, config } => {
            let cfg = Config::load(&config)?;
            let hub = serve_hub(hub).await?;
            attach(hub, Arc::new(Spawner::new(&cfg).await?)).await?;
            std::future::pending::<()>().await;
        }
    }
    Ok(())
}
