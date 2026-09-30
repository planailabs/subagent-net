//! `personality up` runs Vesper; `personality adduser <name>` adds a login.
//! `personality memory <db>` and `personality stt …` are the memory MCP
//! server and the speech stage the cluster file starts.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use personality::{CLUSTER, Opts, stop_postgres, up};
use vesper_room::tts::{self, Tts};
use vesper_room::users::Users;

#[derive(Parser)]
#[command(about = "Vesper, a virtual personality on subagent-net")]
struct Cli {
    #[arg(long, global = true, default_value = "vesper-data")]
    data: PathBuf,
    /// Load environment variables (DEEPSEEK_API_KEY, FIRECRAWL_API_KEY, …)
    /// from this file (repeatable; `./.env` is read too). Variables already
    /// set win.
    #[arg(long, global = true)]
    env_file: Vec<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run everything.
    Up {
        /// Use this Postgres instead of a private one in <data>/pg.
        #[arg(long, env = "DATABASE_URL")]
        database: Option<String>,
        #[arg(long, default_value = "127.0.0.1:8700")]
        room: String,
        #[arg(long, default_value = "127.0.0.1:8780")]
        hub: String,
        #[arg(long, default_value = "127.0.0.1:8790")]
        webhooks: String,
        /// Another OpenAI-compatible endpoint for her mind.
        #[arg(long)]
        llm_url: Option<String>,
        /// Silent voice (her mouth still moves) instead of Piper.
        #[arg(long)]
        silent: bool,
        /// A cluster file other than the built-in cluster.hcl.
        #[arg(long)]
        cluster: Option<PathBuf>,
    },
    /// Add a login, or change its password.
    Adduser {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// The memory MCP server (stdio).
    Memory { db: PathBuf },
    /// The speech-to-text stage.
    Stt(vesper_stt::stage::Args),
}

fn main() -> anyhow::Result<()> {
    // Before parsing (flags have env defaults) and before any thread exists.
    subnet::envfile::load(&std::env::args().collect::<Vec<_>>())?;
    let cli = Cli::parse();
    if matches!(cli.cmd, Cmd::Up { .. }) {
        // SAFETY: still single-threaded; the runtime starts below.
        if std::env::var_os("ROOM_MCP_TOKEN").is_none() {
            unsafe { std::env::set_var("ROOM_MCP_TOKEN", uuid::Uuid::new_v4().simple().to_string()) };
        }
        // The hub's tool router shares the memory server's e5 download.
        if std::env::var_os("SUBNET_MODELS").is_none() {
            unsafe { std::env::set_var("SUBNET_MODELS", std::path::absolute(cli.data.join("models"))?) };
        }
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let quiet = matches!(cli.cmd, Cmd::Memory { .. } | Cmd::Stt(_));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| if quiet { "warn".into() } else { "info,rmcp=warn".into() }))
        .init();
    let data = std::path::absolute(&cli.data)?;
    match cli.cmd {
        Cmd::Memory { db } => vesper_memory::mcp::serve_stdio(&db).await,
        Cmd::Stt(args) => vesper_stt::stage::run(args).await,
        Cmd::Adduser { name, password_stdin } => {
            let password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_string()
            } else {
                rpassword_prompt(&name)?
            };
            let name = Users::open(data.join("users.json"))?.set(&name, &password)?;
            println!("{name} can log in");
            Ok(())
        }
        Cmd::Up { database, room, hub, webhooks, llm_url, silent, cluster } => {
            let tts = if silent || !on_path("piper") {
                if !silent {
                    tracing::warn!("piper isn't on PATH (try `nix develop .#personality`): Vesper's voice is silent");
                }
                Tts::Silent
            } else {
                Tts::Piper { bin: "piper".into(), model: tts::ensure_voice(&data.join("voices"), tts::DEFAULT_VOICE).await? }
            };
            if std::env::var_os("DEEPSEEK_API_KEY").is_none() && llm_url.is_none() {
                tracing::warn!("DEEPSEEK_API_KEY isn't set: Vesper can't think");
            }
            let private_pg = database.is_none();
            let text = match cluster {
                Some(p) => std::fs::read_to_string(p)?,
                None => CLUSTER.to_string(),
            };
            let opts = Opts {
                data: data.clone(),
                database,
                room: room.parse()?,
                hub: hub.parse()?,
                webhooks: webhooks.parse()?,
                llm_url,
                exe: std::env::current_exe()?,
                tts,
                time_scale: 1.0,
                mcp_token: std::env::var("ROOM_MCP_TOKEN")?,
            };
            let r = up(opts, &text).await?;
            if !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist/index.html")).exists() {
                tracing::warn!("the web client isn't built: cd personality-example/web && npm install && npm run build");
            }
            if r.room.users.is_empty() {
                tracing::warn!("no logins yet: `personality adduser <name>`");
            }
            println!("Vesper's room:   {}", r.room_url);
            println!("subnet hub UI:   {}  (admin token {})", r.hub_url, r.admin_token);
            tokio::signal::ctrl_c().await?;
            r.hub.shutdown();
            if private_pg {
                stop_postgres(&data.join("pg"));
            }
            Ok(())
        }
    }
}

fn rpassword_prompt(name: &str) -> anyhow::Result<String> {
    let p = rpassword::prompt_password(format!("password for {name}: "))?;
    anyhow::ensure!(p == rpassword::prompt_password("again: ")?, "the passwords differ");
    Ok(p)
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}
