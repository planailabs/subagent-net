//! `subnet watch <id>`: an agent's transcript, live and readable. Built on
//! the `watch_agent` long-poll; streamed text appears as it's written.

use std::io::Write;

use serde_json::{Value, json};
use subnet_core::addr::AgentId;

use crate::client::Client;

/// Turns `watch_agent` answers into terminal lines.
pub struct Printer {
    color: bool,
    /// Streamed text of the answer in progress, as printed so far.
    streamed: String,
    /// Last state line printed.
    state: String,
    /// Cut shown tool results to this many characters (0 = all).
    pub result_chars: usize,
}

impl Printer {
    pub fn new(color: bool) -> Self {
        Printer { color, streamed: String::new(), state: String::new(), result_chars: 600 }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// What to print for one `watch_agent` answer.
    pub fn render(&mut self, w: &Value) -> String {
        let mut out = String::new();
        for e in w["entries"].as_array().into_iter().flatten() {
            let content = e["content"].as_str().unwrap_or_default();
            match e["role"].as_str().unwrap_or_default() {
                "user" => {
                    self.end_stream(&mut out);
                    out.push_str(&format!("\n{}\n", self.paint("1;36", &format!("▸ {}", content.replace('\n', "\n  ")))));
                }
                "assistant" => {
                    // Whatever was already streamed isn't printed again.
                    let rest = content.strip_prefix(self.streamed.as_str()).unwrap_or(content);
                    if !self.streamed.is_empty() && rest.len() == content.len() && !content.is_empty() {
                        out.push('\n');
                    }
                    if self.streamed.is_empty() && !content.is_empty() {
                        out.push_str(&self.paint("1;35", "◂ "));
                    }
                    out.push_str(rest);
                    if !content.is_empty() || !self.streamed.is_empty() {
                        out.push('\n');
                    }
                    self.streamed.clear();
                    for c in e["tool_calls"].as_array().into_iter().flatten() {
                        let line = format!("  → {} {}", c["name"].as_str().unwrap_or_default(), c["arguments"].as_str().unwrap_or_default());
                        out.push_str(&format!("{}\n", self.paint("33", &line)));
                    }
                }
                "tool" => {
                    self.end_stream(&mut out);
                    let shown: String = if self.result_chars > 0 && content.chars().count() > self.result_chars {
                        format!("{}…", content.chars().take(self.result_chars).collect::<String>())
                    } else {
                        content.to_string()
                    };
                    out.push_str(&format!("{}\n", self.paint("2", &format!("  ← {}", shown.replace('\n', "\n    ")))));
                }
                other => out.push_str(&format!("[{other}] {content}\n")),
            }
        }
        if let Some(p) = w["partial"].as_str() {
            if let Some(rest) = p.strip_prefix(self.streamed.as_str()) {
                if self.streamed.is_empty() {
                    out.push_str(&self.paint("1;35", "◂ "));
                }
                out.push_str(rest);
            } else {
                // A different partial (a new turn after a pause): start over.
                self.end_stream(&mut out);
                out.push_str(&self.paint("1;35", "◂ "));
                out.push_str(p);
            }
            self.streamed = p.to_string();
        }
        let mut state = w["phase"].as_str().unwrap_or("?").to_string();
        if let Some(p) = w["pause"].as_str() {
            state.push_str(&format!(", {} pause{}", p, if w["paused"] == true { "d" } else { " requested" }));
        }
        if let Some(c) = w["awaiting_approval"]["function"]["name"].as_str() {
            state.push_str(&format!(", waiting for approval of {c}"));
        }
        // A stream that stopped without an answer (aborted, failed) ends its line.
        if w["partial"].is_null() {
            self.end_stream(&mut out);
        }
        // State changes get a line, but never in the middle of streamed text.
        if state != self.state {
            if self.streamed.is_empty() {
                out.push_str(&format!("{}\n", self.paint("2", &format!("· {state}"))));
            }
            self.state = state;
        }
        out
    }

    fn end_stream(&mut self, out: &mut String) {
        if !self.streamed.is_empty() {
            out.push('\n');
            self.streamed.clear();
        }
    }
}

/// Follows an agent until interrupted.
pub async fn watch(c: &Client, id: AgentId, tail: u64, mut out: impl Write, color: bool) -> anyhow::Result<()> {
    let mut p = Printer::new(color);
    let mut after: Option<u64> = None;
    loop {
        let w = c.call_raw("watch_agent", json!({"id": id, "after": after, "tail": tail, "timeout_ms": 30_000, "max_chars": 0})).await?;
        write!(out, "{}", p.render(&w))?;
        out.flush()?;
        after = w["next"].as_u64();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(entries: Value, partial: Value, phase: &str) -> Value {
        json!({"entries": entries, "partial": partial, "phase": phase, "pause": null, "paused": false, "awaiting_approval": null})
    }

    #[test]
    fn streams_then_finishes_without_repeating() {
        let mut p = Printer::new(false);
        let out = p.render(&w(json!([{"index": 0, "role": "user", "content": "[message from user:me]\nhi"}]), Value::Null, "idle"));
        assert_eq!(out, "\n▸ [message from user:me]\n  hi\n· idle\n");
        assert_eq!(p.render(&w(json!([]), json!("Hel"), "thinking")), "◂ Hel");
        assert_eq!(p.render(&w(json!([]), json!("Hello th"), "thinking")), "lo th");
        let done = w(json!([{"index": 1, "role": "assistant", "content": "Hello there."}]), Value::Null, "idle");
        assert_eq!(p.render(&done), "ere.\n· idle\n");
    }

    #[test]
    fn tool_calls_and_results() {
        let mut p = Printer::new(false);
        p.result_chars = 5;
        let out = p.render(&w(
            json!([
                {"index": 0, "role": "assistant", "tool_calls": [{"id": "c1", "name": "world.say", "arguments": "{\"text\":\"hi\"}"}]},
                {"index": 1, "role": "tool", "tool_call_id": "c1", "content": "said it (1.2 s)"}
            ]),
            Value::Null,
            "tools",
        ));
        assert_eq!(out, "  → world.say {\"text\":\"hi\"}\n  ← said …\n· tools\n");
    }

    #[test]
    fn state_changes_and_approvals_show() {
        let mut p = Printer::new(false);
        p.render(&w(json!([]), Value::Null, "idle"));
        let mut paused = w(json!([]), Value::Null, "tools");
        paused["pause"] = json!("quick");
        paused["awaiting_approval"] = json!({"function": {"name": "rm"}});
        assert_eq!(p.render(&paused), "· tools, quick pause requested, waiting for approval of rm\n");
        assert_eq!(p.render(&paused), "", "unchanged state isn't repeated");
    }
}
