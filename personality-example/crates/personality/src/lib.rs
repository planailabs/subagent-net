//! Runs Vesper in one process: a Postgres (private, unless given one), the
//! subnet hub and node, the room server, and the cluster file applied.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use subnet::hub::db::ClusterFile;
use subnet::hub::{Hub, http};
use subnet::node::{Node, attach};
use subnet_core::addr::Addr;
use vesper_room::server::{self, Config, Room};
use vesper_room::tts::Tts;

pub const CLUSTER: &str = include_str!("../../../cluster.hcl");
const ROOM_DEFAULT: &str = "127.0.0.1:8700";
const LLM_DEFAULT: &str = "https://api.deepseek.com/v1";

pub struct Opts {
    pub data: PathBuf,
    /// Postgres URL; None starts a private one in `<data>/pg`.
    pub database: Option<String>,
    pub room: SocketAddr,
    pub hub: SocketAddr,
    pub webhooks: SocketAddr,
    /// An OpenAI-compatible endpoint instead of DeepSeek's.
    pub llm_url: Option<String>,
    /// The launcher binary, which also serves `memory` and `stt`.
    pub exe: PathBuf,
    pub tts: Tts,
    pub time_scale: f64,
    /// The world MCP's bearer token; the node reads it from
    /// `ROOM_MCP_TOKEN`, so the two must agree.
    pub mcp_token: String,
}

/// The cluster file with this run's paths and addresses filled in, and
/// without the web (Firecrawl) MCP server in her mixture when there's no
/// `FIRECRAWL_API_KEY`: a spawn needs every server of its mixture live.
pub fn render_cluster(text: &str, o: &Opts, room: SocketAddr) -> String {
    let quoted = |p: &Path| format!("{:?}", p.display().to_string());
    let data = format!("{}/", o.data.display());
    let text = if std::env::var_os("FIRECRAWL_API_KEY").is_some() {
        text.to_string()
    } else {
        tracing::warn!("FIRECRAWL_API_KEY isn't set: Vesper has no web search");
        text.replace(r#"mcp   = ["world", "memory", "web"]"#, r#"mcp   = ["world", "memory"]"#)
    };
    text.replace("[\"personality\",", &format!("[{},", quoted(&o.exe)))
        .replace("vesper-data/", &data)
        .replace(ROOM_DEFAULT, &room.to_string())
        .replace(LLM_DEFAULT, o.llm_url.as_deref().unwrap_or(LLM_DEFAULT))
}

/// A private Postgres cluster in `dir` (started if it isn't running).
/// Shelling out: there is no Rust binding for running a Postgres server.
pub fn private_postgres(dir: &Path) -> anyhow::Result<String> {
    let port_file = dir.join("vesper-port");
    if !dir.join("PG_VERSION").exists() {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir.parent().unwrap_or(Path::new(".")))?;
        let ok = Command::new("initdb")
            .args(["-U", "postgres", "--auth=trust", "-D"])
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|e| anyhow::anyhow!("initdb: {e} (run inside `nix develop`, or pass --database)"))?
            .success();
        anyhow::ensure!(ok, "initdb failed");
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        std::fs::write(&port_file, port.to_string())?;
    }
    let port = std::fs::read_to_string(&port_file)?;
    let running = Command::new("pg_ctl").arg("status").arg("-D").arg(dir).stdout(std::process::Stdio::null()).status()?.success();
    if !running {
        let ok = Command::new("pg_ctl")
            .arg("-D")
            .arg(dir)
            .arg("-l")
            .arg(dir.join("log.txt"))
            .args(["-w", "-o"])
            .arg(format!("-p {port} -h 127.0.0.1 -k ''"))
            .arg("start")
            .stdout(std::process::Stdio::null())
            .status()?
            .success();
        anyhow::ensure!(ok, "pg_ctl start failed, see {}", dir.join("log.txt").display());
    }
    Ok(format!("postgres://postgres@127.0.0.1:{port}/postgres"))
}

pub fn stop_postgres(dir: &Path) {
    let _ = Command::new("pg_ctl").arg("-D").arg(dir).args(["-m", "fast", "stop"]).stdout(std::process::Stdio::null()).status();
}

pub struct Running {
    pub room_url: String,
    pub hub_url: String,
    pub admin_token: String,
    pub hub: Arc<Hub>,
    pub room: Arc<Room>,
}

pub async fn up(o: Opts, cluster: &str) -> anyhow::Result<Running> {
    std::fs::create_dir_all(&o.data)?;
    let db = match &o.database {
        Some(u) => u.clone(),
        None => {
            let dir = o.data.join("pg");
            tokio::task::spawn_blocking(move || private_postgres(&dir)).await??
        }
    };
    // Listeners first: the ports decide what goes into the cluster file.
    let room_l = tokio::net::TcpListener::bind(o.room).await?;
    let hub_l = tokio::net::TcpListener::bind(o.hub).await?;
    let hooks_l = tokio::net::TcpListener::bind(o.webhooks).await?;
    let (room_addr, hub_addr, hooks_addr) = (room_l.local_addr()?, hub_l.local_addr()?, hooks_l.local_addr()?);

    let room = Room::new(Config {
        hooks: Some(format!("http://{hooks_addr}/hooks")),
        time_scale: o.time_scale,
        mcp_token: Some(o.mcp_token.clone()),
        users: o.data.join("users.json"),
        web: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist")),
        assets: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets")),
        tts: o.tts.clone(),
    })?;
    tokio::spawn(server::serve(room.clone(), room_l));

    let admin_token = uuid::Uuid::new_v4().simple().to_string();
    let hub_url = format!("http://{hub_addr}");
    let hub = Hub::start(&db, Some(admin_token.clone()), &hub_url).await?;
    let router = http::router(hub.clone());
    tokio::spawn(async move { axum::serve(hub_l, router).await });
    hub.wait_leader().await;
    let text = render_cluster(cluster, &o, room_addr);
    let applied = hub.apply_cluster(vec![ClusterFile { name: "cluster.hcl".into(), text }], false, &Addr::root()).await?;
    tracing::info!(version = ?applied.version, "cluster applied");

    let mut senses = vec![];
    for name in hub.cluster().spec.nodes.keys() {
        let node = Arc::new(Node::new(name, None));
        senses.push(node.senses.clone());
        attach(hub.clone(), node).await?;
    }
    let hooks = subnet::node::senses::webhook_router(senses);
    tokio::spawn(async move { axum::serve(hooks_l, hooks).await });
    Ok(Running { room_url: format!("http://{room_addr}"), hub_url, admin_token, hub, room })
}
