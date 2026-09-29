//! Hub operations and the hub ↔ spawner wire protocol (JSON over WebSocket).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::addr::{Addr, AgentId};
use crate::agent::{Budget, Event, PauseMode, Spec, Status};

/// An agent type as a spawner offers it. Secrets never leave the spawner, so
/// this is only what the hub needs for placement and child specs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeInfo {
    pub name: String,
    /// Hash of the full type config minus secrets.
    pub hash: String,
    #[serde(default)]
    pub description: String,
    /// Types agents of this type may spawn.
    #[serde(default)]
    pub spawns: Vec<String>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub approve: Vec<String>,
}

impl TypeInfo {
    /// `name@hash`, the identity used in `Spec::ty`.
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.hash)
    }
}

/// Something a participant asks the hub to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Spawn { ty: String, prompt: String },
    Send { to: Addr, content: String },
    ListAgents,
    ListTypes,
    Pause { id: AgentId, mode: PauseMode, #[serde(default)] tree: bool },
    Resume { id: AgentId, #[serde(default)] tree: bool },
    Cancel { id: AgentId },
    Approve { id: AgentId, call_id: String, approved: bool },
    /// Copy an agent's log (up to `at` events) into a new, parentless agent.
    Fork { id: AgentId, #[serde(default)] at: Option<u64> },
    Transcript { id: AgentId },
    /// Block until a message for the caller arrives (for MCP clients).
    WaitInbox { #[serde(default)] timeout_ms: Option<u64> },
}

/// A delivered message in a user/client mailbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mail {
    pub from: Addr,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToHub {
    Hello { name: String, #[serde(default)] token: Option<String>, types: Vec<TypeInfo>, capacity: u32 },
    /// Events the spawner wants committed to an agent's log.
    Propose { agent: AgentId, epoch: u64, events: Vec<Event> },
    /// `Effect::Report`: a turn ended.
    Report { agent: AgentId, epoch: u64, to: Vec<Addr>, status: Status, content: String },
    /// A built-in tool call made by an agent.
    Request { id: u64, agent: AgentId, epoch: u64, op: Op },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToSpawner {
    Welcome,
    Rejected { reason: String },
    /// Run this agent: replay `events`, recover, then follow `Commit`s.
    Assign { agent: AgentId, epoch: u64, spec: Spec, events: Vec<Event> },
    Commit { agent: AgentId, seq: u64, event: Event },
    /// Stop running this agent (moved elsewhere or stale epoch).
    Revoke { agent: AgentId },
    Reply { id: u64, result: Result<Value, String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn op_wire_format() {
        let op: Op = serde_json::from_value(json!({"op":"pause","id":uuid::Uuid::nil(),"mode":"quick"})).unwrap();
        assert_eq!(op, Op::Pause { id: uuid::Uuid::nil(), mode: PauseMode::Quick, tree: false });
        assert_eq!(serde_json::to_value(Op::ListTypes).unwrap(), json!({"op":"list_types"}));
    }

    #[test]
    fn messages_roundtrip() {
        let msgs = vec![
            ToSpawner::Reply { id: 3, result: Err("nope".into()) },
            ToSpawner::Reply { id: 4, result: Ok(json!([1])) },
            ToSpawner::Commit { agent: uuid::Uuid::nil(), seq: 1, event: Event::LlmDone },
        ];
        for m in msgs {
            let j = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<ToSpawner>(&j).unwrap(), m);
        }
        let h = ToHub::Hello { name: "s".into(), token: None, types: vec![], capacity: 1 };
        assert_eq!(serde_json::from_str::<ToHub>(&serde_json::to_string(&h).unwrap()).unwrap(), h);
    }

    #[test]
    fn type_id() {
        let t = TypeInfo {
            name: "coder".into(),
            hash: "ab12".into(),
            description: String::new(),
            spawns: vec![],
            budget: Budget::default(),
            approve: vec![],
        };
        assert_eq!(t.id(), "coder@ab12");
    }
}
