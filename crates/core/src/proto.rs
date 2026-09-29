//! Hub operations agents and clients can ask for, and mail.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::addr::{Addr, AgentId};
use crate::agent::{PauseMode, Status};

/// Something a participant asks the hub to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Spawn {
        ty: String,
        prompt: String,
    },
    Send {
        to: Addr,
        content: String,
    },
    ListAgents,
    ListTypes,
    Pause {
        id: AgentId,
        mode: PauseMode,
        #[serde(default)]
        tree: bool,
    },
    Resume {
        id: AgentId,
        #[serde(default)]
        tree: bool,
    },
    Cancel {
        id: AgentId,
    },
    Approve {
        id: AgentId,
        call_id: String,
        approved: bool,
    },
    /// Copy an agent's log (up to `at` events) into a new, parentless agent
    /// (with `tree`, its descendants too).
    Fork {
        id: AgentId,
        #[serde(default)]
        at: Option<u64>,
        #[serde(default)]
        tree: bool,
    },
    Transcript {
        id: AgentId,
    },
    /// Block until a message for the caller arrives (for MCP clients).
    WaitInbox {
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Take (remove) up to `max` messages from a mailbox.
    MailboxTake { name: String, max: u32 },
    /// Look at up to `max` messages without removing them.
    MailboxPeek { name: String, max: u32 },
}

/// A delivered message in a user/client mailbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Mail {
    pub from: Addr,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
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

}
