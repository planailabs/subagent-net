//! Hub ↔ node protocol: JSON messages over one WebSocket per node.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use subnet_cluster::NodeConfig;
use subnet_core::addr::AgentId;
use subnet_core::agent::{Agent, Event, Spec};
use subnet_core::chat::ToolDef;
use subnet_core::proto::Op;

/// A type this node can or can't run after resolving its secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStatus {
    /// `name@hash`
    pub id: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpStatus {
    pub id: String,
    /// Tools the server lists (unprefixed names).
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub error: Option<String>,
}

/// An agent's folded state after `seq` events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub state: Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToHub {
    Hello {
        name: String,
        #[serde(default)]
        token: Option<String>,
    },
    /// What the node could start from its latest `Configure`.
    Ready { agents: Vec<AgentStatus>, mcps: Vec<McpStatus> },
    /// Events the node wants committed to an agent's log.
    Propose { agent: AgentId, epoch: u64, events: Vec<Event> },
    /// A built-in tool call made by an agent.
    Request { id: u64, agent: AgentId, epoch: u64, op: Op },
    /// An agent's call to an MCP tool hosted on another node.
    McpCall { id: u64, agent: AgentId, epoch: u64, mcp: String, tool: String, args: Value },
    /// Abort an earlier `McpCall` (hard pause).
    McpCancel { id: u64 },
    /// Result of an `McpInvoke` this node ran.
    McpResult { id: u64, result: Result<String, String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToNode {
    Welcome,
    Rejected { reason: String },
    /// What this node should run. Sent after `Welcome` and on every cluster change.
    Configure { config: NodeConfig },
    /// Run this agent: start from `snapshot` (the state after `seq` events) if
    /// any, fold `events` (the last is `Recovered`), then follow `Commit`s.
    Assign {
        agent: AgentId,
        epoch: u64,
        spec: Spec,
        #[serde(default)]
        snapshot: Option<Snapshot>,
        events: Vec<Event>,
    },
    Commit { agent: AgentId, seq: u64, event: Event },
    /// Stop running this agent (moved elsewhere, dormant or stale epoch).
    Revoke { agent: AgentId },
    Reply { id: u64, result: Result<Value, String> },
    /// Run a tool of an MCP type this node hosts, for an agent elsewhere.
    McpInvoke { id: u64, mcp: String, tool: String, args: Value },
    McpAbort { id: u64 },
    /// Result of this node's `McpCall`.
    McpReply { id: u64, result: Result<String, String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrips() {
        let msgs = vec![
            ToNode::Reply { id: 3, result: Err("nope".into()) },
            ToNode::McpInvoke { id: 1, mcp: "m@h".into(), tool: "t".into(), args: json!({"a":1}) },
            ToNode::Commit { agent: uuid::Uuid::nil(), seq: 1, event: Event::LlmDone },
        ];
        for m in msgs {
            let j = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<ToNode>(&j).unwrap(), m);
        }
        let h = ToHub::McpResult { id: 2, result: Ok("x".into()) };
        assert_eq!(serde_json::from_str::<ToHub>(&serde_json::to_string(&h).unwrap()).unwrap(), h);
    }
}
