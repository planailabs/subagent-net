//! The hub's API: every operation defined once for REST/RPC, MCP and the CLI.

use std::sync::Arc;

use axum::http::HeaderMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{PauseMode, Queued};
use subnet_core::chat::{Message, ToolCall, Usage};
use subnet_core::proto::Mail;
use subnet_ops::{Caller, ErrorKind, Method, NoArgs, Op, OpError, OpMeta, Registry, Role};

use crate::hub::auth::{Applied, PrincipalKind};
use crate::hub::db::{ClusterFile, VersionInfo};
use crate::hub::{Hub, HubError};

// ---------- result types ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSummary {
    pub id: AgentId,
    #[serde(rename = "type")]
    pub ty: String,
    pub parent: Option<AgentId>,
    /// idle, thinking, tools, failed or cancelled.
    pub phase: String,
    pub pause: Option<PauseMode>,
    /// Paused and nothing left to finish.
    pub paused: bool,
    /// Node the agent currently runs on; none when dormant or pending.
    pub node: Option<String>,
    pub usage: Usage,
    /// Tokens handed to children.
    pub reserved: u64,
    /// Number of committed events.
    pub seq: u64,
    /// The tool call waiting for approval, if any.
    pub awaiting_approval: Option<ToolCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Transcript {
    #[serde(flatten)]
    pub summary: AgentSummary,
    pub messages: Vec<Message>,
    /// The assistant message being streamed (or cut off by a pause).
    pub partial: Option<Message>,
    /// Messages queued for the next LLM call.
    pub inbox: Vec<Queued>,
}

/// Something that can be spawned: a mixture or a bare agent type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TypeSummary {
    pub name: String,
    /// `mixture` or `agent`.
    pub kind: String,
    /// `name@hash` of the agent type it runs.
    pub id: String,
    pub description: String,
    /// MCP types a mixture binds.
    pub mcp: Vec<String>,
    /// Live nodes offering the agent type.
    pub nodes: u32,
    /// Free agent slots on those nodes.
    pub free: u32,
}

/// A connected node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeSummary {
    pub name: String,
    pub capacity: u32,
    /// Agents it runs now.
    pub running: u32,
    /// Has reported what it can run for the current cluster version.
    pub configured: bool,
    /// Agent types (ids) it can run.
    pub agents: Vec<String>,
    /// MCP types (ids) it runs.
    pub mcps: Vec<String>,
    /// What it couldn't start (id → error), e.g. a missing credential.
    pub errors: std::collections::BTreeMap<String, String>,
    /// Its senses and their last problem (`null` = fine).
    pub senses: std::collections::BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Spawned {
    pub id: AgentId,
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Done {
    pub ok: bool,
}

impl Done {
    pub const OK: Done = Done { ok: true };
}

// ---------- the caller ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub addr: Addr,
    pub role: Role,
}

impl Caller for Principal {
    fn role(&self) -> Role {
        self.role
    }
}

impl From<HubError> for OpError {
    fn from(e: HubError) -> Self {
        let kind = match &e {
            HubError::Bad(_) => ErrorKind::BadRequest,
            HubError::NotFound(_) => ErrorKind::NotFound,
            HubError::Forbidden(_) => ErrorKind::Forbidden,
            HubError::Unauthorized(_) => ErrorKind::Unauthorized,
            HubError::NotLeader => ErrorKind::Unavailable,
            HubError::Db(_) => ErrorKind::Internal,
        };
        OpError::new(kind, e.to_string())
    }
}

// ---------- operations ----------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct IdArgs {
    /// Agent id.
    pub id: AgentId,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SpawnArgs {
    /// Agent type (or mixture) name, see list_types.
    #[serde(rename = "type")]
    pub ty: String,
    /// The task: the agent's first message.
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SendArgs {
    /// Agent id, `user`, or `client:<name>`.
    pub to: Addr,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InboxArgs {
    /// How long to block for mail, in ms (default 30000, max 300000; 0 = don't wait).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PauseArgs {
    pub id: AgentId,
    /// safe: finish the turn; quick: finish the in-flight call; hard: abort now, keep partial output.
    pub mode: PauseMode,
    /// Also pause all descendants.
    #[serde(default)]
    pub tree: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResumeArgs {
    pub id: AgentId,
    /// Also resume all descendants.
    #[serde(default)]
    pub tree: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApproveArgs {
    pub id: AgentId,
    /// The waiting tool call (see awaiting_approval).
    pub call_id: String,
    /// true to run the call, false to deny it.
    pub approved: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ForkArgs {
    pub id: AgentId,
    /// Number of events to copy; default all.
    #[serde(default)]
    pub at: Option<u64>,
    /// Also fork the children it had spawned by then (with their descendants).
    #[serde(default)]
    pub tree: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ApplyArgs {
    /// Cluster files; their blocks are merged.
    pub files: Vec<ClusterFile>,
    /// Only validate and show the changes.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClusterView {
    /// None before the first apply.
    pub version: Option<VersionInfo>,
    pub files: Vec<ClusterFile>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VersionArgs {
    pub version: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PrincipalArgs {
    pub kind: PrincipalKind,
    /// Name as declared in the cluster file.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Token {
    /// Shown once; the hub keeps only its hash.
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Revoked {
    pub revoked: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WhoAmI {
    pub addr: Addr,
    pub role: Role,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeliveriesArgs {
    /// Only this route.
    #[serde(default)]
    pub route: Option<String>,
    /// Newest first; default 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MailArgs {
    /// Any address: `mailbox:<name>`, `route:<name>`, `user:<name>`, …
    pub addr: Addr,
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InjectArgs {
    /// A sense declared in the cluster.
    pub sense: String,
    /// The event data.
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Injected {
    pub id: String,
}

/// A sense from the cluster and how it is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SenseSummary {
    pub name: String,
    pub node: String,
    /// exec, stream-publisher, stream-subscriber, webhook, timer or file.
    pub source: String,
    /// Stages, in order.
    pub stages: Vec<String>,
    /// Its node is connected and reported the sense started.
    pub running: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlobPutArgs {
    pub base64: String,
    #[serde(default = "octet_stream")]
    pub mime: String,
}

fn octet_stream() -> String {
    "application/octet-stream".into()
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BlobGetArgs {
    /// The sha256 (or `blob:<sha256>`).
    pub hash: String,
}

macro_rules! op {
    ($t:ident, $name:literal, $role:ident, $http:expr, $args:ty, $out:ty, $summary:literal) => {
        pub struct $t;
        impl Op for $t {
            const NAME: &'static str = $name;
            const SUMMARY: &'static str = $summary;
            const ROLE: Role = Role::$role;
            const HTTP: Option<(Method, &'static str)> = $http;
            type Args = $args;
            type Out = $out;
        }
    };
}

op!(ListTypes, "list_types", Viewer, Some((Method::Get, "/v1/types")), NoArgs, Vec<TypeSummary>, "List agent types that live nodes offer.");
op!(ListNodes, "list_nodes", Viewer, Some((Method::Get, "/v1/nodes")), NoArgs, Vec<NodeSummary>, "List connected nodes and what they run.");
op!(ListAgents, "list_agents", Viewer, Some((Method::Get, "/v1/agents")), NoArgs, Vec<AgentSummary>, "List all agents with phase, pause state, node and usage.");
op!(GetAgent, "transcript", Viewer, Some((Method::Get, "/v1/agents/{id}")), IdArgs, Transcript, "An agent's transcript, partial output, queued inbox and state.");
op!(Spawn, "spawn", Operator, Some((Method::Post, "/v1/agents")), SpawnArgs, Spawned, "Spawn an agent with a task. Its answer arrives in your inbox.");
op!(Send, "send", Operator, Some((Method::Post, "/v1/messages")), SendArgs, Done, "Send a message to an agent (it answers into your inbox), a user or a client.");
op!(WaitInbox, "wait_inbox", Viewer, Some((Method::Get, "/v1/inbox")), InboxArgs, Vec<Mail>, "Take the messages addressed to you, waiting for some if there are none.");
op!(Pause, "pause", Operator, Some((Method::Post, "/v1/agents/{id}/pause")), PauseArgs, Done, "Pause an agent: safe (finish turn), quick (finish in-flight call) or hard (abort, keep partial output).");
op!(Resume, "resume", Operator, Some((Method::Post, "/v1/agents/{id}/resume")), ResumeArgs, Done, "Resume a paused or failed agent.");
op!(Cancel, "cancel", Operator, Some((Method::Post, "/v1/agents/{id}/cancel")), IdArgs, Done, "Cancel an agent and all its descendants.");
op!(Approve, "approve", Operator, Some((Method::Post, "/v1/agents/{id}/approve")), ApproveArgs, Done, "Approve or deny the tool call an agent is waiting on.");
op!(ApplyCluster, "apply_cluster", Admin, Some((Method::Put, "/v1/cluster")), ApplyArgs, Applied, "Validate cluster files and make them the desired state (or just diff with dry_run).");
op!(GetCluster, "get_cluster", Viewer, Some((Method::Get, "/v1/cluster")), NoArgs, ClusterView, "The applied cluster files and their version.");
op!(ClusterHistory, "cluster_history", Viewer, Some((Method::Get, "/v1/cluster/history")), NoArgs, Vec<VersionInfo>, "All applied cluster versions, newest first.");
op!(RollbackCluster, "rollback_cluster", Admin, Some((Method::Post, "/v1/cluster/rollback")), VersionArgs, Applied, "Re-apply an earlier cluster version as a new version.");
op!(IssueToken, "issue_token", Admin, Some((Method::Post, "/v1/tokens")), PrincipalArgs, Token, "Create a token for a user, client or node declared in the cluster.");
op!(RevokeTokens, "revoke_tokens", Admin, Some((Method::Post, "/v1/tokens/revoke")), PrincipalArgs, Revoked, "Revoke all tokens of a principal.");
op!(WhoAmIOp, "whoami", Viewer, Some((Method::Get, "/v1/whoami")), NoArgs, WhoAmI, "Who the hub thinks you are.");
op!(ListRoutes, "list_routes", Viewer, Some((Method::Get, "/v1/routes")), NoArgs, Vec<crate::hub::switchboard::RouteSummary>, "Switchboard routes with their counters.");
op!(ListDeliveries, "list_deliveries", Viewer, Some((Method::Get, "/v1/deliveries")), DeliveriesArgs, Vec<crate::hub::db::DeliveryRow>, "Recent switchboard deliveries and their outcomes.");
op!(ListSenses, "list_senses", Viewer, Some((Method::Get, "/v1/senses")), NoArgs, Vec<SenseSummary>, "Senses of the cluster and whether they run.");
op!(PeekMail, "peek_mail", Operator, Some((Method::Get, "/v1/mail")), MailArgs, Vec<Mail>, "Pending messages at any address (not taken).");
op!(InjectEvent, "inject_event", Operator, Some((Method::Post, "/v1/senses/{sense}/events")), InjectArgs, Injected, "Feed an event into the switchboard as if the sense had produced it.");
op!(BlobPut, "blob_put", Operator, Some((Method::Post, "/v1/blobs")), BlobPutArgs, crate::hub::blobs::BlobRef, "Store a blob; returns its blob:<sha256> reference.");
op!(BlobGetOp, "blob_get", Viewer, Some((Method::Get, "/v1/blobs/{hash}")), BlobGetArgs, crate::hub::blobs::Blob, "Read a blob as base64 (raw bytes: GET /v1/blobs/{hash}/raw).");
op!(Fork, "fork", Operator, Some((Method::Post, "/v1/agents/{id}/fork")), ForkArgs, Spawned, "Copy an agent's history (optionally only the first `at` events, optionally with its children) into a new agent.");

/// Metadata of every operation (what the CLI needs; no hub required).
pub fn metas() -> Vec<OpMeta> {
    vec![
        OpMeta::of::<ListTypes>(),
        OpMeta::of::<ListNodes>(),
        OpMeta::of::<ListAgents>(),
        OpMeta::of::<GetAgent>(),
        OpMeta::of::<Spawn>(),
        OpMeta::of::<Send>(),
        OpMeta::of::<WaitInbox>(),
        OpMeta::of::<Pause>(),
        OpMeta::of::<Resume>(),
        OpMeta::of::<Cancel>(),
        OpMeta::of::<Approve>(),
        OpMeta::of::<Fork>(),
        OpMeta::of::<ApplyCluster>(),
        OpMeta::of::<GetCluster>(),
        OpMeta::of::<ClusterHistory>(),
        OpMeta::of::<RollbackCluster>(),
        OpMeta::of::<IssueToken>(),
        OpMeta::of::<RevokeTokens>(),
        OpMeta::of::<WhoAmIOp>(),
        OpMeta::of::<ListRoutes>(),
        OpMeta::of::<ListDeliveries>(),
        OpMeta::of::<ListSenses>(),
        OpMeta::of::<PeekMail>(),
        OpMeta::of::<InjectEvent>(),
        OpMeta::of::<BlobPut>(),
        OpMeta::of::<BlobGetOp>(),
    ]
}

pub fn registry(hub: Arc<Hub>) -> Registry<Principal> {
    let mut r = Registry::<Principal>::new();
    let h = hub.clone();
    r.add::<ListTypes, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.list_types().await) }
    });
    let h = hub.clone();
    r.add::<ListNodes, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.list_nodes().await) }
    });
    let h = hub.clone();
    r.add::<ListAgents, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.list_agents().await) }
    });
    let h = hub.clone();
    r.add::<GetAgent, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(h.transcript(a.id).await?) }
    });
    let h = hub.clone();
    r.add::<Spawn, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.spawn(&c.addr, &a.ty, a.prompt).await?) }
    });
    let h = hub.clone();
    r.add::<Send, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.send(&c.addr, a.to, a.content).await?) }
    });
    let h = hub.clone();
    r.add::<WaitInbox, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.wait_inbox(&c.addr, a.timeout_ms).await?) }
    });
    let h = hub.clone();
    r.add::<Pause, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.pause(&c.addr, a.id, a.mode, a.tree).await?) }
    });
    let h = hub.clone();
    r.add::<Resume, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.resume(&c.addr, a.id, a.tree).await?) }
    });
    let h = hub.clone();
    r.add::<Cancel, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.cancel(&c.addr, a.id).await?) }
    });
    let h = hub.clone();
    r.add::<Approve, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.approve(&c.addr, a.id, a.call_id, a.approved).await?) }
    });
    let h = hub.clone();
    r.add::<Fork, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.fork(&c.addr, a.id, a.at, a.tree).await?) }
    });
    let h = hub.clone();
    r.add::<ApplyCluster, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.apply_cluster(a.files, a.dry_run, &c.addr).await?) }
    });
    let h = hub.clone();
    r.add::<GetCluster, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move {
            let c = h.cluster();
            Ok(ClusterView { version: c.version, files: c.files })
        }
    });
    let h = hub.clone();
    r.add::<ClusterHistory, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.cluster_history().await?) }
    });
    let h = hub.clone();
    r.add::<RollbackCluster, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.rollback_cluster(a.version, &c.addr).await?) }
    });
    let h = hub.clone();
    r.add::<IssueToken, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(Token { token: h.issue_token(a.kind, &a.name).await? }) }
    });
    let h = hub.clone();
    r.add::<RevokeTokens, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(Revoked { revoked: h.revoke_tokens(a.kind, &a.name).await? }) }
    });
    r.add::<WhoAmIOp, _, _>(|c, _: NoArgs| async move { Ok(WhoAmI { addr: c.addr, role: c.role }) });
    let h = hub.clone();
    r.add::<ListRoutes, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.list_routes().await) }
    });
    let h = hub.clone();
    r.add::<ListDeliveries, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(h.list_deliveries(a.route.as_deref(), a.limit.unwrap_or(100)).await?) }
    });
    let h = hub.clone();
    r.add::<ListSenses, _, _>(move |_, _: NoArgs| {
        let h = h.clone();
        async move { Ok(h.list_senses().await) }
    });
    let h = hub.clone();
    r.add::<PeekMail, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(h.peek_mail(&a.addr, a.max.unwrap_or(50)).await?) }
    });
    let h = hub.clone();
    r.add::<InjectEvent, _, _>(move |_, a| {
        let h = h.clone();
        async move { Ok(Injected { id: h.inject_event(&a.sense, a.data)? }) }
    });
    let h = hub.clone();
    r.add::<BlobPut, _, _>(move |_, a| {
        let h = h.clone();
        async move {
            let data = crate::hub::blobs::decode_b64(&a.base64)?;
            Ok(h.put_blob(&data, &a.mime).await?)
        }
    });
    let h = hub;
    r.add::<BlobGetOp, _, _>(move |_, a| {
        let h = h.clone();
        async move {
            let (mime, data) = h.get_blob(&a.hash).await?;
            Ok(crate::hub::blobs::Blob { mime, size: data.len() as u64, base64: crate::hub::blobs::encode_b64(&data) })
        }
    });
    r
}

/// Bearer token → principal (see `Hub::authenticate`).
pub fn auth(hub: Arc<Hub>) -> subnet_ops::http::Auth<Principal> {
    Arc::new(move |h: HeaderMap| {
        let hub = hub.clone();
        Box::pin(async move {
            let bearer = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
            Ok(hub.authenticate(bearer)?)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metas_cover_every_op_once() {
        let names: Vec<_> = metas().iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate op names");
        // Every REST path parameter is a field of the arguments.
        for m in metas() {
            for p in m.path_params() {
                assert!(m.args["properties"].get(p).is_some(), "{}: {p}", m.name);
            }
        }
    }
}
