use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use subnet::api::metas;
use subnet::client::{Client, tail};
use subnet::hub::{Hub, http};
use subnet::hub::db::ClusterFile;
use subnet::node::{Node, attach};
use subnet_core::addr::AgentId;
use subnet_ops::cli::{args_from, command_name, commands};

#[derive(Parser)]
#[command(version, about = "Distributed network of resumable LLM agents")]
struct Cli {
    /// Hub HTTP address(es), comma-separated (for client commands).
    #[arg(long, global = true, env = "SUBNET_HUB", default_value = "http://127.0.0.1:7700")]
    hub: String,
    /// Your token for the hub.
    #[arg(long, global = true, env = "SUBNET_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct HubArgs {
    #[arg(long, env = "DATABASE_URL")]
    db: String,
    #[arg(long, env = "SUBNET_LISTEN", default_value = "127.0.0.1:7700")]
    listen: String,
    /// Bootstrap token of the built-in `user:root` admin. Without it the hub
    /// runs in open mode: every caller is root (development only).
    #[arg(long, env = "SUBNET_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    /// URL other hubs' standbys point clients to when this hub leads
    /// (default: http://<listen>).
    #[arg(long, env = "SUBNET_ADVERTISE")]
    advertise: Option<String>,
}

/// Commands that aren't API operations. Every operation is added as a
/// subcommand generated from its definition.
#[derive(Subcommand)]
enum Cmd {
    /// Run the hub.
    Hub(HubArgs),
    /// Run a node: connects to the hub(s) and runs its part of the cluster.
    Node {
        /// This node's name as declared in the cluster file.
        #[arg(long, env = "SUBNET_NODE")]
        name: String,
        /// Serve webhook senses (`POST /hooks/<path>`) on this address.
        #[arg(long, env = "SUBNET_WEBHOOKS")]
        webhooks: Option<String>,
    },
    /// Run a hub, apply cluster files, and run every node they declare, all in
    /// one process.
    Dev {
        #[command(flatten)]
        hub: HubArgs,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Serve webhook senses of all nodes on this address.
        #[arg(long)]
        webhooks: Option<String>,
    },
    /// Stream events live (all agents, or one).
    Tail {
        id: Option<AgentId>,
        /// Include the agent's descendants.
        #[arg(long, requires = "id")]
        tree: bool,
    },
    /// The agent park and an agent pane in the terminal.
    Tui,
    /// Apply cluster files (HCL) to the hub.
    Apply {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Only validate and show the changes.
        #[arg(long)]
        dry_run: bool,
    },
}

async fn serve_hub(a: HubArgs) -> anyhow::Result<Arc<Hub>> {
    let advertise = a.advertise.clone().unwrap_or_else(|| format!("http://{}", a.listen));
    // Serves as a standby until elected; several hubs can share the database.
    let hub = Hub::start(&a.db, a.admin_token, &advertise).await?;
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

async fn serve_webhooks(addr: &str, senses: Vec<Arc<subnet::node::senses::Senses>>) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "webhooks listening");
    let app = subnet::node::senses::webhook_router(senses);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(l, app).await {
            tracing::error!(error = %e, "webhook server stopped");
        }
    });
    Ok(())
}

fn read_files(paths: &[PathBuf]) -> anyhow::Result<Vec<ClusterFile>> {
    paths
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
            Ok(ClusterFile { name: p.display().to_string(), text })
        })
        .collect()
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

/// Waits for the next message from `from` and prints its content.
async fn await_answer(c: &Client, from: &str) -> anyhow::Result<()> {
    loop {
        let mail = c.call_raw("wait_inbox", json!({"timeout_ms": 300_000})).await?;
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
    let agent = v["agent"].as_str().unwrap_or("-").get(..8).unwrap_or("-").to_string();
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

fn cli() -> clap::Command {
    let wait = Arg::new("wait").long("wait").short('w').action(ArgAction::SetTrue).help("Wait for the answer and print it");
    Cli::command().subcommands(commands(&metas()).into_iter().map(|c| match c.get_name() {
        "spawn" | "send" => c.arg(wait.clone()),
        _ => c,
    }))
}

async fn run_op(name: &str, sub: &ArgMatches, c: &Client) -> anyhow::Result<()> {
    let meta = metas().into_iter().find(|m| command_name(m.name) == name).expect("generated command");
    let args = args_from(&meta, sub).map_err(anyhow::Error::msg)?;
    let wait = sub.try_get_one::<bool>("wait").ok().flatten().copied().unwrap_or(false);
    let out = c.call_raw(meta.name, args.clone()).await?;
    match (meta.name, wait) {
        ("spawn", true) => await_answer(c, &format!("agent:{}", out["id"].as_str().unwrap_or_default())).await,
        ("send", true) => {
            let to = args["to"].as_str().unwrap_or_default();
            let from = if to.contains(':') || to == "user" { to.to_string() } else { format!("agent:{to}") };
            await_answer(c, &from).await
        }
        _ => {
            print(&out);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let m = cli().get_matches();
    let (name, sub) = m.subcommand().expect("subcommand required");
    let builtin = matches!(name, "hub" | "node" | "dev" | "tail" | "apply" | "tui");
    // Servers log their work; client commands only problems.
    let default = if matches!(name, "hub" | "node" | "dev") { "info,rmcp=warn" } else { "warn" };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into()))
        .init();
    let hub_url = m.get_one::<String>("hub").cloned().unwrap_or_default();
    let token = m.get_one::<String>("token").cloned();
    if !builtin {
        return run_op(name, sub, &Client::new(&hub_url, token)).await;
    }
    let cli = Cli::from_arg_matches(&m)?;
    match cli.cmd {
        Cmd::Hub(a) => {
            serve_hub(a).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Node { name, webhooks } => {
            let node = Arc::new(Node::new(&name, cli.token));
            if let Some(addr) = webhooks {
                serve_webhooks(&addr, vec![node.senses.clone()]).await?;
            }
            subnet::node::ws::run(node, &cli.hub).await?;
        }
        Cmd::Dev { hub, files, webhooks } => {
            let files = read_files(&files)?;
            let hub = serve_hub(hub).await?;
            hub.wait_leader().await;
            let applied = hub.apply_cluster(files, false, &subnet_core::addr::Addr::root()).await?;
            tracing::info!(version = ?applied.version, "cluster applied");
            let mut senses = vec![];
            for name in hub.cluster().spec.nodes.keys() {
                let node = Arc::new(Node::new(name, None));
                senses.push(node.senses.clone());
                attach(hub.clone(), node).await?;
            }
            if let Some(addr) = webhooks {
                serve_webhooks(&addr, senses).await?;
            }
            std::future::pending::<()>().await;
        }
        Cmd::Tail { id, tree } => tail(&cli.hub, cli.token.as_deref(), id, tree, show_event).await?,
        Cmd::Tui => subnet::tui::run(Client::new(&cli.hub, cli.token.clone()), cli.hub.clone(), cli.token).await?,
        Cmd::Apply { files, dry_run } => {
            let files = read_files(&files)?;
            let c = Client::new(&cli.hub, cli.token);
            print(&c.call_raw("apply_cluster", json!({"files": files, "dry_run": dry_run})).await?);
        }
    }
    Ok(())
}
