use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use subnet::api::{AS_HEADER, metas};
use subnet::client::{Client, tail};
use subnet::hub::{Hub, http};
use subnet::spawner::{Spawner, attach, config::Config};
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

/// Commands that aren't API operations. Every operation is added as a
/// subcommand generated from its definition.
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
    let builtin = matches!(name, "hub" | "spawner" | "dev" | "tail");
    // Servers log their work; client commands only problems.
    let default = if builtin && name != "tail" { "info,rmcp=warn" } else { "warn" };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into()))
        .init();
    let hub_url = m.get_one::<String>("hub").cloned().unwrap_or_default();
    let token = m.get_one::<String>("token").cloned();
    let who = m.get_one::<String>("who").cloned().unwrap_or_else(|| "user".into());
    if !builtin {
        let c = Client::new(&hub_url, token).with_header(AS_HEADER, &who);
        return run_op(name, sub, &c).await;
    }
    let cli = Cli::from_arg_matches(&m)?;
    match cli.cmd {
        Cmd::Hub(a) => {
            serve_hub(a, cli.token).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Spawner { config } => {
            let cfg = Config::load(&config)?;
            let spawner = Arc::new(Spawner::new(&cfg).await?);
            subnet::spawner::ws::run(spawner, &cfg.hub).await?;
        }
        Cmd::Dev { hub, config } => {
            let cfg = Config::load(&config)?;
            let hub = serve_hub(hub, cli.token).await?;
            attach(hub, Arc::new(Spawner::new(&cfg).await?)).await?;
            std::future::pending::<()>().await;
        }
        Cmd::Tail { id } => tail(&cli.hub, cli.token.as_deref(), id, show_event).await?,
    }
    Ok(())
}
