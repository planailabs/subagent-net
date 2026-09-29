//! OpenAI-compatible chat types. Unknown fields are kept in `extra` so provider
//! extensions (reasoning content, cache info, …) survive a round trip.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Message {
    fn text(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: Some(content.into()), tool_calls: vec![], tool_call_id: None, extra: Map::new() }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::text(Role::System, c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::text(Role::User, c)
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self::text(Role::Assistant, c)
    }
    pub fn tool(call_id: impl Into<String>, c: impl Into<String>) -> Self {
        Self { tool_call_id: Some(call_id.into()), ..Self::text(Role::Tool, c) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".into()
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
        Self { id: id.into(), kind: function_type(), function: FunctionCall { name: name.into(), arguments: arguments.into() } }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, exactly as the model produced them.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON schema of the arguments.
    pub parameters: Value,
}

/// One streamed chunk, already reduced to the first choice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallDelta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// Folds deltas into an assistant message. Whatever has arrived so far is a
/// valid partial: that is what gets kept on pause/abort.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Accumulator {
    pub content: String,
    pub tool_calls: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

impl Accumulator {
    pub fn push(&mut self, d: &Delta) {
        if let Some(c) = &d.content {
            self.content.push_str(c);
        }
        for tc in &d.tool_calls {
            let slot = match self.tool_calls.iter_mut().find(|t| t.index == tc.index) {
                Some(s) => s,
                None => {
                    self.tool_calls.push(ToolCallDelta { index: tc.index, ..Default::default() });
                    self.tool_calls.last_mut().unwrap()
                }
            };
            if tc.id.is_some() {
                slot.id.clone_from(&tc.id);
            }
            if let Some(n) = &tc.name {
                slot.name.get_or_insert_default().push_str(n);
            }
            if let Some(a) = &tc.arguments {
                slot.arguments.get_or_insert_default().push_str(a);
            }
        }
        if d.finish_reason.is_some() {
            self.finish_reason.clone_from(&d.finish_reason);
        }
        if d.usage.is_some() {
            self.usage = d.usage;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
    }

    /// The assistant message. Tool calls are included only when `complete`;
    /// a partial tool call (half-streamed JSON) must never be executed.
    fn message(&self, complete: bool) -> Message {
        let mut m = Message::assistant(self.content.clone());
        if self.content.is_empty() {
            m.content = None;
        }
        if complete {
            m.tool_calls = self
                .tool_calls
                .iter()
                .map(|t| {
                    ToolCall::new(
                        t.id.clone().unwrap_or_else(|| format!("call_{}", t.index)),
                        t.name.clone().unwrap_or_default(),
                        t.arguments.clone().unwrap_or_default(),
                    )
                })
                .collect();
        }
        m
    }

    /// Message for a stream that ended normally.
    pub fn finish(&self) -> Message {
        self.message(true)
    }

    /// Text-only partial for a stream that was cut off.
    pub fn partial(&self) -> Message {
        self.message(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tc(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> Delta {
        Delta {
            tool_calls: vec![ToolCallDelta {
                index,
                id: id.map(Into::into),
                name: name.map(Into::into),
                arguments: args.map(Into::into),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn message_roundtrip_keeps_unknown_fields() {
        let v = json!({"role":"assistant","content":"hi","reasoning_content":"hmm"});
        let m: Message = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(m.extra["reasoning_content"], "hmm");
        assert_eq!(serde_json::to_value(&m).unwrap(), v);
    }

    #[test]
    fn tool_message_serializes_call_id() {
        let v = serde_json::to_value(Message::tool("c1", "ok")).unwrap();
        assert_eq!(v, json!({"role":"tool","content":"ok","tool_call_id":"c1"}));
    }

    #[test]
    fn tool_call_defaults_type() {
        let t: ToolCall = serde_json::from_value(json!({"id":"a","function":{"name":"f","arguments":"{}"}})).unwrap();
        assert_eq!(t.kind, "function");
    }

    #[test]
    fn accumulates_text() {
        let mut a = Accumulator::default();
        for s in ["Hel", "lo", "!"] {
            a.push(&Delta { content: Some(s.into()), ..Default::default() });
        }
        assert_eq!(a.finish().content.as_deref(), Some("Hello!"));
    }

    #[test]
    fn accumulates_interleaved_tool_calls() {
        let mut a = Accumulator::default();
        a.push(&tc(0, Some("c0"), Some("read"), Some("{\"p\":")));
        a.push(&tc(1, Some("c1"), Some("write"), Some("{}")));
        a.push(&tc(0, None, None, Some("1}")));
        a.push(&Delta { finish_reason: Some("tool_calls".into()), ..Default::default() });
        let m = a.finish();
        assert_eq!(m.content, None);
        assert_eq!(m.tool_calls, vec![ToolCall::new("c0", "read", "{\"p\":1}"), ToolCall::new("c1", "write", "{}")]);
        assert_eq!(a.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn partial_drops_tool_calls_keeps_text() {
        let mut a = Accumulator::default();
        a.push(&Delta { content: Some("let me".into()), ..Default::default() });
        a.push(&tc(0, Some("c0"), Some("read"), Some("{\"p")));
        let p = a.partial();
        assert_eq!(p.content.as_deref(), Some("let me"));
        assert!(p.tool_calls.is_empty());
    }

    #[test]
    fn missing_tool_call_id_gets_synthesized() {
        let mut a = Accumulator::default();
        a.push(&tc(2, None, Some("f"), Some("{}")));
        assert_eq!(a.finish().tool_calls[0].id, "call_2");
    }

    #[test]
    fn keeps_last_usage() {
        let mut a = Accumulator::default();
        a.push(&Delta { usage: Some(Usage { prompt_tokens: 3, completion_tokens: 4 }), ..Default::default() });
        assert_eq!(a.usage.unwrap().total(), 7);
    }

    #[test]
    fn empty_accumulator() {
        let a = Accumulator::default();
        assert!(a.is_empty());
        assert_eq!(a.partial().content, None);
    }
}
