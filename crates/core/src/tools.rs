//! Built-in network tools every agent gets.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::addr::{Addr, AgentId};
use crate::agent::{PauseMode, WAIT_FOR};
use crate::chat::{ToolCall, ToolDef};
use crate::proto::Op;

fn def(name: &str, description: &str, parameters: Value) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), parameters }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":props,"required":required})
}

pub fn builtin_tools() -> Vec<ToolDef> {
    let id = json!({"type":"string","description":"agent id"});
    let mode = json!({"type":"string","enum":["safe","quick","hard"],
        "description":"safe: finish the current turn; quick: finish the in-flight call; hard: abort now"});
    vec![
        def(
            "spawn_agent",
            "Start a sub-agent of the given type with a task. Returns its id immediately; its final answer arrives as a report.",
            obj(json!({"type":{"type":"string"},"prompt":{"type":"string"}}), &["type", "prompt"]),
        ),
        def(
            "send_message",
            "Send a message to an agent (agent id), a resident (\"resident:<name>\"), a user (\"user:<name>\"), a client (\"client:<name>\") or a mailbox (\"mailbox:<name>\").",
            obj(json!({"to":{"type":"string"},"content":{"type":"string"}}), &["to", "content"]),
        ),
        def(
            WAIT_FOR,
            "Block until each of the given child agents has reported, then return their reports.",
            obj(json!({"ids":{"type":"array","items":{"type":"string"}}}), &["ids"]),
        ),
        def("list_agents", "List agents in the network with their state.", obj(json!({}), &[])),
        def("list_types", "List agent types that can be spawned.", obj(json!({}), &[])),
        def(
            "pause_agent",
            "Pause one of your descendant agents (tree = also its descendants).",
            obj(json!({"id":id,"mode":mode,"tree":{"type":"boolean"}}), &["id", "mode"]),
        ),
        def(
            "resume_agent",
            "Resume a paused descendant agent.",
            obj(json!({"id":id,"tree":{"type":"boolean"}}), &["id"]),
        ),
        def("cancel_agent", "Cancel a descendant agent and its subtree.", obj(json!({"id":id}), &["id"])),
        def(
            "mailbox_take",
            "Take (and remove) up to `max` messages from a mailbox your mixture may read.",
            obj(json!({"name":{"type":"string"},"max":{"type":"integer","minimum":1}}), &["name"]),
        ),
        def(
            "blob_get",
            "Read a blob referred to as blob:<sha256> (text as-is, other data as base64; long blobs are cut).",
            obj(json!({"ref":{"type":"string"}}), &["ref"]),
        ),
        def(
            "mailbox_peek",
            "Look at up to `max` messages in a mailbox without removing them.",
            obj(json!({"name":{"type":"string"},"max":{"type":"integer","minimum":1}}), &["name"]),
        ),
    ]
}

/// The built-in tool of an agent type with `search_history`: its whole
/// conversation, summarised parts too.
pub fn history_tool() -> ToolDef {
    def(
        "search_history",
        "Search your whole conversation, the parts summarised away too, for words or a regex (case-insensitive): the matching messages, oldest first, 20 a page, each with its number, its role and whether it's from before a summary.",
        obj(json!({"pattern":{"type":"string"},"page":{"type":"integer","minimum":1}}), &["pattern"]),
    )
}

/// The built-in tool of an agent type with `grep_results`: searching a
/// tool result that reached it cut (or any of its tool results).
pub fn grep_tool() -> ToolDef {
    def(
        "grep_result",
        "One of your tool results, all of it (a long one reaches you cut; `call` is the tool call's id, as the cut note says), searched or read. With `pattern`: words or a regex (case-insensitive), the matching lines with their numbers and `context` lines around (default 2), 30 matches a page. Or read it by lines: `from` and `to` (line numbers, inclusive; `from` alone: 50 lines), at most 200 lines and long lines cut, with where to read on; `full: true` gives the lines uncut and as many as asked, or, without a range, the whole result (to read a book, say).",
        obj(json!({"call":{"type":"string"},"pattern":{"type":"string"},"context":{"type":"integer","minimum":0,"maximum":20},"page":{"type":"integer","minimum":1},"from":{"type":"integer","minimum":1},"to":{"type":"integer","minimum":1},"full":{"type":"boolean"}}), &["call"]),
    )
}

/// Lines `from` to `to` of a text (1-based, inclusive; no `to`: 50 lines,
/// or with `full` to the end), for `grep_result`'s reading. Unless `full`:
/// at most 200 lines, each cut at 2000 characters, and 30 000 in all.
/// `{lines, from, to, next, text: [{line, text}]}` (`next`: where to read on).
pub fn read_lines(text: &str, from: usize, to: Option<usize>, full: bool) -> Result<Value, String> {
    const LINE: usize = 2000;
    const TOTAL: usize = 30_000;
    const MAX_LINES: usize = 200;
    let all: Vec<&str> = text.lines().collect();
    let start = from.max(1);
    if start > all.len().max(1) {
        return Err(format!("it has {} lines", all.len()));
    }
    let end = match (to, full) {
        (Some(t), _) => t,
        (None, true) => all.len(),
        (None, false) => start + 49,
    }
    .min(all.len());
    let want = if full { end + 1 - start } else { (end + 1 - start).min(MAX_LINES) };
    let (mut shown, mut size) = (vec![], 0);
    for (k, l) in all.iter().enumerate().skip(start - 1).take(want) {
        let cut = if full { l.len() } else { l.floor_char_boundary(LINE.min(l.len())) };
        let text = if cut < l.len() { format!("{}…", &l[..cut]) } else { l.to_string() };
        if !full && size + text.len() > TOTAL && !shown.is_empty() {
            break;
        }
        size += text.len();
        shown.push(json!({"line": k + 1, "text": text}));
    }
    let last = start - 1 + shown.len();
    Ok(json!({"text": shown, "from": start, "to": last, "lines": all.len(), "next": (last < all.len()).then_some(last + 1)}))
}

/// Maps a built-in tool call to a hub op. `None` = not a built-in (an MCP tool).
pub fn builtin_op(call: &ToolCall) -> Option<Result<Op, String>> {
    fn parse<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T, String> {
        // Some models send "" for no-argument calls.
        let args = if args.trim().is_empty() { "{}" } else { args };
        serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))
    }
    #[derive(Deserialize)]
    struct Spawn {
        #[serde(rename = "type")]
        ty: String,
        prompt: String,
    }
    #[derive(Deserialize)]
    struct Send {
        to: Addr,
        content: String,
    }
    #[derive(Deserialize)]
    struct Pause {
        id: AgentId,
        mode: PauseMode,
        #[serde(default)]
        tree: bool,
    }
    #[derive(Deserialize)]
    struct Resume {
        id: AgentId,
        #[serde(default)]
        tree: bool,
    }
    #[derive(Deserialize)]
    struct Id {
        id: AgentId,
    }
    #[derive(Deserialize)]
    struct BlobArg {
        #[serde(rename = "ref")]
        reference: String,
    }
    #[derive(Deserialize)]
    struct Mailbox {
        name: String,
        #[serde(default = "ten")]
        max: u32,
    }
    fn ten() -> u32 {
        10
    }
    #[derive(Deserialize)]
    struct Search {
        pattern: String,
        #[serde(default = "one")]
        page: u32,
    }
    fn one() -> u32 {
        1
    }
    #[derive(Deserialize)]
    struct Grep {
        call: String,
        #[serde(default)]
        pattern: Option<String>,
        #[serde(default)]
        from: Option<u32>,
        #[serde(default)]
        to: Option<u32>,
        #[serde(default)]
        full: bool,
        #[serde(default = "two")]
        context: u32,
        #[serde(default = "one")]
        page: u32,
    }
    fn two() -> u32 {
        2
    }

    let a = &call.function.arguments;
    Some(match call.function.name.as_str() {
        "spawn_agent" => parse::<Spawn>(a).map(|s| Op::Spawn { ty: s.ty, prompt: s.prompt, tenant: None }),
        "send_message" => parse::<Send>(a).map(|s| Op::Send { to: s.to, content: s.content }),
        "list_agents" => Ok(Op::ListAgents),
        "list_types" => Ok(Op::ListTypes),
        "pause_agent" => parse::<Pause>(a).map(|p| Op::Pause { id: p.id, mode: p.mode, tree: p.tree }),
        "resume_agent" => parse::<Resume>(a).map(|r| Op::Resume { id: r.id, tree: r.tree }),
        "cancel_agent" => parse::<Id>(a).map(|i| Op::Cancel { id: i.id }),
        "blob_get" => parse::<BlobArg>(a).map(|b| Op::BlobGet { reference: b.reference }),
        "mailbox_take" => parse::<Mailbox>(a).map(|m| Op::MailboxTake { name: m.name, max: m.max.max(1) }),
        "mailbox_peek" => parse::<Mailbox>(a).map(|m| Op::MailboxPeek { name: m.name, max: m.max.max(1) }),
        "search_history" => parse::<Search>(a).map(|s| Op::SearchHistory { pattern: s.pattern, page: s.page.max(1) }),
        "grep_result" => parse::<Grep>(a).and_then(|g| {
            let reading = g.from.is_some() || g.to.is_some() || g.full;
            match g.pattern.filter(|p| !p.is_empty()) {
                Some(_) if reading => Err("a pattern to search, or lines to read (from, to, full): not both".into()),
                Some(p) => Ok(Op::GrepResult { call: g.call, pattern: p, context: g.context.min(20), page: g.page.max(1) }),
                None if !reading => Err("give a pattern to search for, or lines to read: from (and to), or full".into()),
                None if g.to.is_some_and(|t| t < g.from.unwrap_or(1)) => Err("to comes before from".into()),
                None => Ok(Op::ReadResult { call: g.call, from: g.from.unwrap_or(1).max(1), to: g.to, full: g.full }),
            }
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall::new("c", name, args)
    }

    #[test]
    fn every_builtin_except_wait_maps_to_op() {
        let id = Uuid::new_v4();
        let args = json!({"type":"t","prompt":"p","to":"user:u","content":"c","id":id,"mode":"hard","name":"box","ref":"blob:x"}).to_string();
        for t in builtin_tools() {
            let r = builtin_op(&call(&t.name, &args));
            if t.name == WAIT_FOR {
                assert!(r.is_none());
            } else {
                assert!(r.unwrap().is_ok(), "{}", t.name);
            }
        }
    }

    #[test]
    fn spawn_args() {
        assert_eq!(
            builtin_op(&call("spawn_agent", r#"{"type":"coder","prompt":"go"}"#)).unwrap().unwrap(),
            Op::Spawn { ty: "coder".into(), prompt: "go".into(), tenant: None }
        );
    }

    #[test]
    fn mailbox_max_defaults_and_clamps() {
        assert_eq!(builtin_op(&call("mailbox_take", r#"{"name":"b"}"#)).unwrap().unwrap(), Op::MailboxTake { name: "b".into(), max: 10 });
        assert_eq!(builtin_op(&call("mailbox_peek", r#"{"name":"b","max":0}"#)).unwrap().unwrap(), Op::MailboxPeek { name: "b".into(), max: 1 });
        assert_eq!(builtin_op(&call("search_history", r#"{"pattern":"tea"}"#)).unwrap().unwrap(), Op::SearchHistory { pattern: "tea".into(), page: 1 });
        assert_eq!(builtin_op(&call("search_history", r#"{"pattern":"tea","page":0}"#)).unwrap().unwrap(), Op::SearchHistory { pattern: "tea".into(), page: 1 });
        let book: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        let r = read_lines(&book, 10, Some(12), false).unwrap();
        assert_eq!((r["from"].as_u64(), r["to"].as_u64(), r["next"].as_u64(), r["lines"].as_u64()), (Some(10), Some(12), Some(13), Some(500)));
        assert_eq!(r["text"][2], json!({"line": 12, "text": "line 12"}));
        assert_eq!(r.as_object().unwrap().keys().last().map(String::as_str), Some("next"), "where to read on comes last");
        let r = read_lines(&book, 490, None, false).unwrap();
        assert_eq!((r["to"].as_u64(), r["next"].is_null()), (Some(500), true), "to the end, nothing after");
        let r = read_lines(&book, 1, Some(400), false).unwrap();
        assert_eq!(r["to"], 200, "at most 200 lines unless full");
        let r = read_lines(&book, 1, None, true).unwrap();
        assert_eq!((r["to"].as_u64(), r["text"].as_array().unwrap().len()), (Some(500), 500), "full: the whole of it");
        let long = "x".repeat(5000);
        assert_eq!(read_lines(&long, 1, None, false).unwrap()["text"][0]["text"].as_str().unwrap().chars().count(), 2001);
        assert_eq!(read_lines(&long, 1, None, true).unwrap()["text"][0]["text"].as_str().unwrap().len(), 5000);
        assert!(read_lines(&book, 501, None, false).unwrap_err().contains("500 lines"));
        let grep = |args: &str| builtin_op(&call("grep_result", args)).unwrap();
        assert_eq!(grep(r#"{"call":"c1","pattern":"tea"}"#).unwrap(), Op::GrepResult { call: "c1".into(), pattern: "tea".into(), context: 2, page: 1 });
        assert_eq!(grep(r#"{"call":"c1","from":10,"to":20}"#).unwrap(), Op::ReadResult { call: "c1".into(), from: 10, to: Some(20), full: false });
        assert_eq!(grep(r#"{"call":"c1","full":true}"#).unwrap(), Op::ReadResult { call: "c1".into(), from: 1, to: None, full: true });
        assert!(grep(r#"{"call":"c1"}"#).unwrap_err().contains("give a pattern"));
        assert!(grep(r#"{"call":"c1","pattern":"x","from":3}"#).unwrap_err().contains("not both"));
        assert!(grep(r#"{"call":"c1","from":9,"to":3}"#).unwrap_err().contains("before"));
    }

    #[test]
    fn empty_args_for_no_arg_tools() {
        assert_eq!(builtin_op(&call("list_agents", "")).unwrap().unwrap(), Op::ListAgents);
    }

    #[test]
    fn bad_args_are_errors() {
        assert!(builtin_op(&call("send_message", r#"{"to":"nobody"}"#)).unwrap().is_err());
        assert!(builtin_op(&call("pause_agent", r#"{"id":"x","mode":"soft"}"#)).unwrap().is_err());
    }

    #[test]
    fn unknown_tool_is_not_builtin() {
        assert!(builtin_op(&call("read_file", "{}")).is_none());
    }

    #[test]
    fn schemas_are_objects() {
        for t in builtin_tools() {
            assert_eq!(t.parameters["type"], "object", "{}", t.name);
        }
    }
}
