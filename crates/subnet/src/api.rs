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
    pub spawner: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TypeSummary {
    pub name: String,
    /// `name@hash`
    pub id: String,
    pub description: String,
    /// Live nodes offering it.
    pub spawners: u32,
    /// Free agent slots on those nodes.
    pub free: u32,
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
op!(ListAgents, "list_agents", Viewer, Some((Method::Get, "/v1/agents")), NoArgs, Vec<AgentSummary>, "List all agents with phase, pause state, node and usage.");
op!(GetAgent, "transcript", Viewer, Some((Method::Get, "/v1/agents/{id}")), IdArgs, Transcript, "An agent's transcript, partial output, queued inbox and state.");
op!(Spawn, "spawn", Operator, Some((Method::Post, "/v1/agents")), SpawnArgs, Spawned, "Spawn an agent with a task. Its answer arrives in your inbox.");
op!(Send, "send", Operator, Some((Method::Post, "/v1/messages")), SendArgs, Done, "Send a message to an agent (it answers into your inbox), a user or a client.");
op!(WaitInbox, "wait_inbox", Viewer, Some((Method::Get, "/v1/inbox")), InboxArgs, Vec<Mail>, "Take the messages addressed to you, waiting for some if there are none.");
op!(Pause, "pause", Operator, Some((Method::Post, "/v1/agents/{id}/pause")), PauseArgs, Done, "Pause an agent: safe (finish turn), quick (finish in-flight call) or hard (abort, keep partial output).");
op!(Resume, "resume", Operator, Some((Method::Post, "/v1/agents/{id}/resume")), ResumeArgs, Done, "Resume a paused or failed agent.");
op!(Cancel, "cancel", Operator, Some((Method::Post, "/v1/agents/{id}/cancel")), IdArgs, Done, "Cancel an agent and all its descendants.");
op!(Approve, "approve", Operator, Some((Method::Post, "/v1/agents/{id}/approve")), ApproveArgs, Done, "Approve or deny the tool call an agent is waiting on.");
op!(Fork, "fork", Operator, Some((Method::Post, "/v1/agents/{id}/fork")), ForkArgs, Spawned, "Copy an agent's history (optionally only the first `at` events) into a new agent.");

/// Metadata of every operation (what the CLI needs; no hub required).
pub fn metas() -> Vec<OpMeta> {
    vec![
        OpMeta::of::<ListTypes>(),
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
    let h = hub;
    r.add::<Fork, _, _>(move |c, a| {
        let h = h.clone();
        async move { Ok(h.fork(&c.addr, a.id, a.at).await?) }
    });
    r
}

/// Header naming who a token holder acts as (until named principals exist).
pub const AS_HEADER: &str = "x-subnet-as";

/// Token auth: the hub token (if any) grants admin; the caller's address comes
/// from `x-subnet-as` (`user` or a client name), else the MCP session id.
pub fn auth(hub: Arc<Hub>) -> subnet_ops::http::Auth<Principal> {
    Arc::new(move |h: HeaderMap| {
        let hub = hub.clone();
        Box::pin(async move {
            let header = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()).map(String::from);
            let bearer = header("authorization").and_then(|v| v.strip_prefix("Bearer ").map(String::from));
            if !hub.token_ok(bearer.as_deref()) {
                return Err(OpError::new(ErrorKind::Unauthorized, "missing or wrong token"));
            }
            let addr = match header(AS_HEADER).as_deref() {
                None | Some("user") => match header("mcp-session-id") {
                    Some(s) if header(AS_HEADER).is_none() => Addr::Client(s),
                    _ => Addr::User,
                },
                Some(name) => Addr::Client(name.into()),
            };
            Ok(Principal { addr, role: Role::Admin })
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
