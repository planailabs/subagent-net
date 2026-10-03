//! Hooks: decisions made outside an agent at points of its own loop (before
//! a tool runs, after it ran, before a message reaches the model, when a
//! turn ends, before a compaction). A hook run is an effect (`RunHook`) and
//! its answer an event (`HookDone`): replays fold the answer and never run
//! the hook again; a hook that was running when its runner died is run
//! again if it's idempotent, else its `on_lost` decides. No I/O here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where in an agent's loop a hook decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    /// A tool call is about to run (`spawn_agent` too): allow, deny, ask a
    /// user, or rewrite its arguments.
    PreTool,
    /// A tool call returned: pass its result, replace it, withhold it, or add a note.
    PostTool,
    /// A message is about to reach the model: pass it, drop it, rewrite it, or add a note.
    OnMessage,
    /// The model ended its turn: let it end, or continue with a message.
    OnTurnEnd,
    /// The turn's answer is about to go out: pass it or rewrite it.
    OnReport,
    /// A compaction is about to run: add instructions for its summary.
    PreCompact,
}

impl HookPoint {
    pub fn name(self) -> &'static str {
        match self {
            HookPoint::PreTool => "pre_tool",
            HookPoint::PostTool => "post_tool",
            HookPoint::OnMessage => "on_message",
            HookPoint::OnTurnEnd => "on_turn_end",
            HookPoint::OnReport => "on_report",
            HookPoint::PreCompact => "pre_compact",
        }
    }

    /// The decisions a hook at this point may answer with.
    pub fn allows(self, d: Decision) -> bool {
        use Decision::*;
        match self {
            HookPoint::PreTool => matches!(d, Allow | Deny | Ask | Rewrite),
            HookPoint::PostTool | HookPoint::OnMessage => matches!(d, Allow | Deny | Rewrite),
            HookPoint::OnTurnEnd => matches!(d, Allow | Continue),
            HookPoint::OnReport => matches!(d, Allow | Rewrite),
            HookPoint::PreCompact => matches!(d, Allow),
        }
    }
}

/// What a hook does when its answer is lost (its runner died and it isn't
/// idempotent), late (timeout), broken or an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnLost {
    /// As if it allowed.
    Allow,
    /// The careful side: a tool call is denied, a result withheld, a message
    /// dropped; a turn just ends, an answer goes out unchanged.
    #[default]
    Deny,
    /// `pre_tool` only: a user decides (an approval).
    Ask,
    /// The agent fails (a tool call first gets the hook's error as its result).
    Fail,
}

/// Where a hook runs (the hub runs it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookRun {
    /// A tool of an MCP server, on any node running it.
    Mcp { server: String, tool: String },
    /// An HTTP endpoint the hub POSTs to.
    Url { url: String },
    /// An agent (a mixture or type) asked to decide: its answer is the outcome.
    Spawn { mixture: String },
}

/// A hook as an agent's spec has it (from the cluster's `hook` block).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HookSpec {
    pub name: String,
    pub on: HookPoint,
    /// Which tools (pre/post_tool) or senders (on_message) it's for:
    /// patterns with `*`; empty is all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<String>,
    /// CEL over `point`, `input` and `agent`, judged by the runner: false
    /// allows without running the hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    pub run: HookRun,
    pub timeout_ms: u64,
    #[serde(default)]
    pub on_lost: OnLost,
    /// Safe to run again after its runner died.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent: bool,
    /// `on_turn_end`: how often it may continue one turn.
    #[serde(default = "max_continue")]
    pub max_continue: u32,
}

fn max_continue() -> u32 {
    3
}

/// A hook's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
    /// New arguments (`args`, pre_tool) or text (`text`: a result, a message, an answer).
    Rewrite,
    /// on_turn_end: go on with `text` as a message.
    Continue,
    /// It couldn't decide (failed, timed out, answered nonsense): its `on_lost` decides.
    Error,
}

/// A hook's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Outcome {
    pub decision: Decision,
    /// Why (shown to the model with a denial; with an error, what went wrong).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// pre_tool rewrite: the arguments to run with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Rewrite: the new text; continue: the message; pre_compact: instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Allow: something to add (to a result, a message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Outcome {
    pub fn allow() -> Self {
        Outcome { decision: Decision::Allow, reason: None, args: None, text: None, note: None }
    }

    pub fn error(reason: impl Into<String>) -> Self {
        Outcome { decision: Decision::Error, reason: Some(reason.into()), args: None, text: None, note: None }
    }

    /// The outcome that stands in for a lost or failed one, by `on_lost`.
    pub fn instead(self, h: &HookSpec) -> Outcome {
        let why = self.reason.unwrap_or_else(|| "it didn't answer".into());
        let decision = match h.on_lost {
            OnLost::Allow => Decision::Allow,
            OnLost::Ask if h.on == HookPoint::PreTool => Decision::Ask,
            OnLost::Deny | OnLost::Ask => match h.on {
                HookPoint::PreTool | HookPoint::PostTool | HookPoint::OnMessage => Decision::Deny,
                _ => Decision::Allow,
            },
            OnLost::Fail => Decision::Error,
        };
        Outcome { decision, reason: Some(format!("hook {} couldn't decide: {why}", h.name)), args: None, text: None, note: None }
    }
}

/// `*` matches any run of characters; everything else itself.
pub fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || !s[first.len()..].ends_with(last) || s.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

/// The hooks of `hooks` at `point` for `subject` (a tool name, a sender),
/// as indices, in order.
pub fn at(hooks: &[HookSpec], point: HookPoint, subject: &str) -> Vec<usize> {
    hooks
        .iter()
        .enumerate()
        .filter(|(_, h)| h.on == point && (h.matches.is_empty() || h.matches.iter().any(|m| glob(m, subject))))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("shell.*", "shell.run") && !glob("shell.*", "web.search"));
        assert!(glob("*", "") && glob("a*c", "abc") && glob("a*c", "ac") && !glob("a*c", "ab"));
        assert!(glob("route:chat-*", "route:chat-to-vesper") && glob("*.delete", "memory.delete"));
        assert!(glob("a*b*c", "a-b-c") && !glob("a*b*c", "a-c") && glob("exact", "exact") && !glob("exact", "exactly"));
    }

    #[test]
    fn lost_outcomes_follow_on_lost_for_the_point() {
        let h = |on, on_lost| HookSpec { name: "h".into(), on, matches: vec![], when: None, run: HookRun::Url { url: "u".into() }, timeout_ms: 1, on_lost, idempotent: false, max_continue: 3 };
        let d = |on, l| Outcome::error("x").instead(&h(on, l)).decision;
        assert_eq!(d(HookPoint::PreTool, OnLost::Deny), Decision::Deny);
        assert_eq!(d(HookPoint::OnTurnEnd, OnLost::Deny), Decision::Allow, "a turn just ends");
        assert_eq!(d(HookPoint::PreTool, OnLost::Ask), Decision::Ask);
        assert_eq!(d(HookPoint::OnMessage, OnLost::Ask), Decision::Deny, "only a tool call can be asked about");
        assert_eq!(d(HookPoint::PostTool, OnLost::Allow), Decision::Allow);
        assert_eq!(d(HookPoint::PreCompact, OnLost::Fail), Decision::Error);
        assert!(Outcome::error("timed out").instead(&h(HookPoint::PreTool, OnLost::Deny)).reason.unwrap().contains("timed out"));
        assert!(HookPoint::OnTurnEnd.allows(Decision::Continue) && !HookPoint::PreTool.allows(Decision::Continue));
    }
}
