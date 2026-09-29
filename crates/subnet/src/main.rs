use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use subnet::client::{Remote, tail};
use subnet::hub::{Hub, http};
use subnet::spawner::{Spawner, attach, config::Config};
use subnet_core::addr::AgentId;

#[derive(Parser)]
#[command(version, about = "Distributed network of resumable LLM agents")]
struct Cli {
    /// Hub HTTP address (for client commands).
    #[arg(long, global = true, env = "SUBNET_HUB", default_value = "http://127.0.0.1:7700")]
    hub: String,
    /// Shared secret of the hub.
    #[arg(long, global = true, env = "SUBNET_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Who you are to the network: `user` or a client name.
    #[arg(long = "as", global = true, env = "SUBNET_AS", default_value = "user")]
    who: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct HubArgs {
    #[arg(long, env = "DATABASE_URL")]
    db: String,
    #[arg(long, env = "SUBNET_LISTEN", default_value = "127.0.0.1:7700")]
    listen: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Mode {
    Safe,
    Quick,
    Hard,
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
    /// List agent types on offer.
    Types,
    /// List agents.
    Agents,
    /// Spawn an agent.
    Spawn {
        #[arg(value_name = "TYPE")]
        ty: String,
        prompt: String,
        /// Wait for its answer and print it.
        #[arg(long, short)]
        wait: bool,
    },
    /// Send a message to an agent (id), `user` or `client:<name>`.
    Send {
        to: String,
        content: String,
        /// Wait for the answer and print it.
        #[arg(long, short)]
        wait: bool,
    },
    /// Show (and wait for) messages addressed to you.
    Inbox {
        #[arg(long, default_value_t = 0)]
        timeout_ms: u64,
    },
    /// Pause an agent: safe (finish turn), quick (finish in-flight call), hard (abort now).
    Pause {
        id: AgentId,
        #[arg(long, value_enum, default_value = "quick")]
        mode: Mode,
        /// Also pause descendants.
        #[arg(long)]
        tree: bool,
    },
    /// Resume a paused or failed agent.
    Resume {
        id: AgentId,
        #[arg(long)]
        tree: bool,
    },
    /// Cancel an agent and its subtree.
    Cancel { id: AgentId },
    /// Approve (or --deny) a tool call an agent waits on.
    Approve {
        id: AgentId,
        call_id: String,
        #[arg(long)]
        deny: bool,
    },
    /// Copy an agent's history into a new agent.
    Fork {
        id: AgentId,
        #[arg(long)]
        at: Option<u64>,
    },
    /// Show an agent's transcript and state.
    Transcript { id: AgentId },
    /// Stream events live (all agents, or one).
    Tail { id: Option<AgentId> },
}

async fn serve_hub(a: HubArgs, token: Option<String>) -> anyhow::Result<Arc<Hub>> {
    let hub = Hub::open(&a.db, token).await?;
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

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

/// Waits for the next message from `from` and prints its content.
async fn await_answer(r: &Remote, from: &str) -> anyhow::Result<()> {
    loop {
        let mail = r.call("wait_inbox", json!({"timeout_ms": 300_000})).await?;
        for m in mail.as_array().into_iter().flatten() {
            if m["from"] == from {
                println!("{}", m["content"].as_str().unwrap_or_default());
                return Ok(());
            }
            eprintln!("(message from {}: {})", m["from"], m["content"]);
        }
    }
}

/// One line per event; streamed text inline.
fn show_event(v: Value) {
    let agent = v["agent"].as_str().unwrap_or("?").get(..8).unwrap_or("?").to_string();
    let ev = &v["event"];
    let mut out = std::io::stdout().lock();
    match ev["type"].as_str() {
        Some("llm_delta") => {
            if let Some(c) = ev["delta"]["content"].as_str() {
                let _ = write!(out, "{c}");
            }
        }
        Some("llm_done") => {
            let _ = writeln!(out);
        }
        Some(t) => {
            let _ = writeln!(out, "\n[{agent}] {t} {}", serde_json::to_string(ev).unwrap());
        }
        None => {
            let _ = writeln!(out, "{v}");
        }
    }
    let _ = out.flush();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let token = cli.token.clone();
    let remote = || Remote::connect(&cli.hub, cli.token.as_deref(), &cli.who);
    let mode = |m: Mode| match m {
        Mode::Safe => "safe",
        Mode::Quick => "quick",
        Mode::Hard => "hard",
    };
    match cli.cmd {
        Cmd::Hub(a) => {
            serve_hub(a, token).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Spawner { config } => {
            let cfg = Config::load(&config)?;
            let spawner = Arc::new(Spawner::new(&cfg).await?);
            subnet::spawner::ws::run(spawner, &cfg.hub).await?;
        }
        Cmd::Dev { hub, config } => {
            let cfg = Config::load(&config)?;
            let hub = serve_hub(hub, token).await?;
            attach(hub, Arc::new(Spawner::new(&cfg).await?)).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Types => print(&remote().await?.call("list_types", Value::Null).await?),
        Cmd::Agents => print(&remote().await?.call("list_agents", Value::Null).await?),
        Cmd::Spawn { ty, prompt, wait } => {
            let r = remote().await?;
            let v = r.call("spawn", json!({"type": ty, "prompt": prompt})).await?;
            if wait {
                await_answer(&r, &format!("agent:{}", v["id"].as_str().unwrap_or_default())).await?;
            } else {
                print(&v);
            }
        }
        Cmd::Send { to, content, wait } => {
            let r = remote().await?;
            let v = r.call("send", json!({"to": to, "content": content})).await?;
            if wait {
                let from = if to.contains(':') || to == "user" { to } else { format!("agent:{to}") };
                await_answer(&r, &from).await?;
            } else {
                print(&v);
            }
        }
        Cmd::Inbox { timeout_ms } => {
            print(&remote().await?.call("wait_inbox", json!({"timeout_ms": timeout_ms})).await?)
        }
        Cmd::Pause { id, mode: m, tree } => {
            print(&remote().await?.call("pause", json!({"id": id, "mode": mode(m), "tree": tree})).await?)
        }
        Cmd::Resume { id, tree } => print(&remote().await?.call("resume", json!({"id": id, "tree": tree})).await?),
        Cmd::Cancel { id } => print(&remote().await?.call("cancel", json!({"id": id})).await?),
        Cmd::Approve { id, call_id, deny } => {
            print(&remote().await?.call("approve", json!({"id": id, "call_id": call_id, "approved": !deny})).await?)
        }
        Cmd::Fork { id, at } => print(&remote().await?.call("fork", json!({"id": id, "at": at})).await?),
        Cmd::Transcript { id } => print(&remote().await?.call("transcript", json!({"id": id})).await?),
        Cmd::Tail { id } => tail(&cli.hub, cli.token.as_deref(), id, show_event).await?,
    }
    Ok(())
}
