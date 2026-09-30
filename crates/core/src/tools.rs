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
