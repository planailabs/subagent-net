//! The hub as an MCP server: any MCP client (another agent, Claude Code, the
//! CLI) can drive the network. The caller's address comes from the
//! `x-subnet-as` header (`user` or a client name), else the MCP session id.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::Deserialize;
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::PauseMode;
use subnet_core::proto::Op;

use super::Hub;

pub const AS_HEADER: &str = "x-subnet-as";

#[derive(Clone)]
pub struct HubMcp {
    hub: Arc<Hub>,
}

pub fn service(hub: Arc<Hub>) -> StreamableHttpService<HubMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(HubMcp { hub: hub.clone() }),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    )
}

fn caller(ctx: &RequestContext<RoleServer>) -> Addr {
    let Some(parts) = ctx.extensions.get::<Parts>() else { return Addr::Client("anonymous".into()) };
    let header = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty());
    match header(AS_HEADER) {
        Some("user") => Addr::User,
        Some(name) => Addr::Client(name.into()),
        None => Addr::Client(header("mcp-session-id").unwrap_or("anonymous").into()),
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SpawnArgs {
    /// Agent type name (see list_types).
    #[serde(rename = "type")]
    ty: String,
    /// The task / first message.
    prompt: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// Agent id, "user" or "client:<name>".
    to: String,
    content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Safe,
    Quick,
    Hard,
}

impl From<Mode> for PauseMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Safe => PauseMode::Safe,
            Mode::Quick => PauseMode::Quick,
            Mode::Hard => PauseMode::Hard,
        }
    }
}

/// Agent ids travel as strings in schemas; parsed here.
fn id(s: &str) -> Result<AgentId, String> {
    s.trim_start_matches("agent:").parse().map_err(|e| format!("bad agent id {s:?}: {e}"))
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PauseArgs {
    id: String,
    /// safe: finish the turn; quick: finish the in-flight call; hard: abort now.
    mode: Mode,
    /// Also pause all descendants.
    #[serde(default)]
    tree: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ResumeArgs {
    id: String,
    #[serde(default)]
    tree: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct IdArgs {
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ApproveArgs {
    id: String,
    call_id: String,
    approved: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ForkArgs {
    id: String,
    /// Number of events to keep; default all.
    #[serde(default)]
    at: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WaitArgs {
    /// How long to block, default 30000, max 300000.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl HubMcp {
    async fn run(&self, ctx: &RequestContext<RoleServer>, op: Op) -> Result<String, String> {
        self.hub.op(&caller(ctx), op).await.map(|v| serde_json::to_string_pretty(&v).unwrap())
    }
}

#[tool_router(server_handler)]
impl HubMcp {
    #[tool(description = "List agent types that live spawners offer.")]
    async fn list_types(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        self.run(&ctx, Op::ListTypes).await
    }

    #[tool(description = "List all agents with phase, pause state, spawner and usage.")]
    async fn list_agents(&self, ctx: RequestContext<RoleServer>) -> Result<String, String> {
        self.run(&ctx, Op::ListAgents).await
    }

    #[tool(description = "Spawn an agent of a type with a task. Its answer arrives in your inbox (wait_inbox).")]
    async fn spawn(
        &self,
        Parameters(a): Parameters<SpawnArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Spawn { ty: a.ty, prompt: a.prompt }).await
    }

    #[tool(description = "Send a message to an agent (it answers into your inbox), the user or a client.")]
    async fn send(
        &self,
        Parameters(a): Parameters<SendArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        let to = a.to.parse()?;
        self.run(&ctx, Op::Send { to, content: a.content }).await
    }

    #[tool(description = "Block until messages for you arrive (or timeout) and return them.")]
    async fn wait_inbox(
        &self,
        Parameters(a): Parameters<WaitArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::WaitInbox { timeout_ms: a.timeout_ms }).await
    }

    #[tool(
        description = "Pause an agent: safe (finish turn), quick (finish in-flight call) or hard (abort, keep partial output)."
    )]
    async fn pause(
        &self,
        Parameters(a): Parameters<PauseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Pause { id: id(&a.id)?, mode: a.mode.into(), tree: a.tree }).await
    }

    #[tool(description = "Resume a paused or failed agent.")]
    async fn resume(
        &self,
        Parameters(a): Parameters<ResumeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Resume { id: id(&a.id)?, tree: a.tree }).await
    }

    #[tool(description = "Cancel an agent and all its descendants.")]
    async fn cancel(
        &self,
        Parameters(a): Parameters<IdArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Cancel { id: id(&a.id)? }).await
    }

    #[tool(description = "Approve or deny a tool call an agent is waiting on.")]
    async fn approve(
        &self,
        Parameters(a): Parameters<ApproveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Approve { id: id(&a.id)?, call_id: a.call_id, approved: a.approved }).await
    }

    #[tool(description = "Copy an agent's history (optionally only the first `at` events) into a new agent.")]
    async fn fork(
        &self,
        Parameters(a): Parameters<ForkArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Fork { id: id(&a.id)?, at: a.at }).await
    }

    #[tool(description = "An agent's transcript, partial output, queued inbox and state.")]
    async fn transcript(
        &self,
        Parameters(a): Parameters<IdArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, String> {
        self.run(&ctx, Op::Transcript { id: id(&a.id)? }).await
    }
}
