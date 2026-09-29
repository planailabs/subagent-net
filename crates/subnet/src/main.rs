use clap::{Parser, Subcommand};
use subnet::hub::{Hub, http};

#[derive(Parser)]
#[command(version, about = "Distributed network of resumable LLM agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the hub.
    Hub {
        #[arg(long, env = "DATABASE_URL")]
        db: String,
        #[arg(long, env = "SUBNET_LISTEN", default_value = "127.0.0.1:7700")]
        listen: String,
        /// Shared secret spawners and clients must present.
        #[arg(long, env = "SUBNET_TOKEN")]
        token: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match Cli::parse().cmd {
        Cmd::Hub { db, listen, token } => {
            let hub = Hub::open(&db, token).await?;
            let l = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!(%listen, "hub listening");
            axum::serve(l, http::router(hub)).await?;
        }
    }
    Ok(())
}
