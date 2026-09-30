//! `vesper-room serve` runs the room; `vesper-room adduser <name>` adds a
//! login (or changes its password).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use vesper_room::server::{self, Config, Room};
use vesper_room::tts::{self, Tts};
use vesper_room::users::Users;

#[derive(Parser)]
struct Cli {
    /// Users, voices.
    #[arg(long, global = true, default_value = "vesper-data")]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum TtsKind {
    /// Piper if it's on PATH, else silent (with a warning).
    Auto,
    Piper,
    Silent,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
        #[arg(long, default_value = "127.0.0.1:8700")]
        listen: String,
        /// The subnet node's webhook base, e.g. http://127.0.0.1:8790/hooks.
        #[arg(long)]
        hooks: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        tts: TtsKind,
        /// Piper voice, downloaded into <data>/voices on first use.
        #[arg(long, default_value = tts::DEFAULT_VOICE)]
        voice: String,
        /// Bearer token the MCP endpoint requires.
        #[arg(long, env = "ROOM_MCP_TOKEN")]
        mcp_token: Option<String>,
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist"))]
        web: PathBuf,
        #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets"))]
        assets: PathBuf,
    },
    /// Add a user, or change their password. Reads the password from the
    /// terminal, or a line on stdin with --password-stdin.
    Adduser {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let users = cli.data.join("users.json");
    match cli.cmd {
        Cmd::Adduser { name, password_stdin } => {
            let password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_string()
            } else {
                let p = rpassword::prompt_password(format!("password for {name}: "))?;
                anyhow::ensure!(p == rpassword::prompt_password("again: ")?, "the passwords differ");
                p
            };
            let name = Users::open(users)?.set(&name, &password)?;
            println!("{name} can log in");
        }
        Cmd::Serve { listen, hooks, tts: kind, voice, mcp_token, web, assets } => {
            let piper = match kind {
                TtsKind::Silent => false,
                TtsKind::Piper => true,
                TtsKind::Auto if on_path("piper") => true,
                TtsKind::Auto => {
                    tracing::warn!("piper isn't on PATH: Vesper's voice is silent (her mouth still moves)");
                    false
                }
            };
            let tts = if piper { Tts::Piper { bin: "piper".into(), model: tts::ensure_voice(&cli.data.join("voices"), &voice).await? } } else { Tts::Silent };
            let room = Room::new(Config { hooks, time_scale: 1.0, mcp_token, users, web, assets, tts })?;
            if room.users.is_empty() {
                tracing::warn!("no users yet: add one with `vesper-room adduser <name>`");
            }
            let l = tokio::net::TcpListener::bind(&listen).await?;
            tracing::info!("the room is at http://{}", l.local_addr()?);
            server::serve(room, l).await?;
        }
    }
    Ok(())
}
