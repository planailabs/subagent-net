//! Hub + nodes + scripted LLM, all in-process.

mod common;

use std::time::Duration;

use common::llm::{last_tool_result, text, tool_call};
use common::net::{Net, agent, nodes};
use serde_json::{Value, json};
use subnet_core::addr::Addr;
use subnet_core::agent::PauseMode;
use subnet_core::proto::Op;

const BOSS: &str = "You are the boss.";
const WORKER: &str = "You are a worker.";

/// boss (may spawn workers) and worker on nodes a, b and s; `worker_extra`
/// adds attributes to the worker type.
fn cluster(worker_extra: &str) -> String {
    let all = ["a", "b", "s"];
    format!(
        "{}{}{}",
        nodes(&all),
        agent(
            "boss",
            BOSS,
            "{llm}",
            &all,
            "  spawns = [\"worker\"]\n  budget = { max_tokens = 100000, max_depth = 1, max_children = 4 }"
        ),
        agent("worker", WORKER, "{llm}", &all, worker_extra),
    )
}

async fn net() -> Net {
    Net::new(&cluster("")).await
}

#[tokio::test]
async fn user_gets_the_answer() {
    let n = net().await;
    n.node("s").await;
    n.llm.say(WORKER, &["Hello", " there"]);
    let id = n.spawn("worker", "hi").await;
    let m = n.mail().await;
    assert_eq!(m["content"], "Hello there");
    assert_eq!(m["from"], format!("agent:{id}"));
    assert_eq!(m["status"], "idle");
    let req = &n.llm.requests()[0];
    assert_eq!(req["messages"][0], json!({"role":"system","content":WORKER}));
    assert_eq!(req["messages"][1], json!({"role":"user","content":"hi"}));
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "spawn_agent"));
    let t = n.t(id).await;
    assert_eq!(t["usage"]["completion_tokens"], 5);
}

#[tokio::test]
async fn conversation_continues_with_follow_ups() {
    let n = net().await;
    n.node("s").await;
    n.llm.say(WORKER, &["one"]);
    n.llm.say(WORKER, &["two"]);
    let id = n.spawn("worker", "count").await;
    assert_eq!(n.mail().await["content"], "one");
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Agent(id), content: "again".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "two");
    let msgs = n.llm.requests()[1]["messages"].as_array().unwrap().len();
    assert_eq!(msgs, 4, "system, user, assistant, user");
}

#[tokio::test]
async fn builtin_tool_round_trip() {
    let n = net().await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "list_types", json!({})));
    n.llm.push(WORKER, |body| {
        let types: Value = serde_json::from_str(&last_tool_result(body)).unwrap();
        let names: Vec<_> = types.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        text(&[&names.join(",")])
    });
    n.spawn("worker", "what types exist?").await;
    assert_eq!(n.mail().await["content"], "boss,worker");
}

#[tokio::test]
async fn unknown_tool_is_reported_to_the_model() {
    let n = net().await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "teleport", json!({})));
    n.llm.push(WORKER, |body| text(&[&last_tool_result(body)]));
    n.spawn("worker", "go").await;
    let c = n.mail().await["content"].as_str().unwrap().to_string();
    assert!(c.contains("unknown tool"), "{c}");
}

#[tokio::test]
async fn boss_spawns_worker_and_waits_for_it() {
    let n = net().await;
    n.node("s").await;
    n.llm.push(BOSS, |_| tool_call("s1", "spawn_agent", json!({"type":"worker","prompt":"compute 6*7"})));
    n.llm.push(BOSS, |body| {
        let id = serde_json::from_str::<Value>(&last_tool_result(body)).unwrap()["id"].as_str().unwrap().to_string();
        tool_call("w1", "wait_for", json!({"ids":[id]}))
    });
    n.llm.push(BOSS, |body| {
        let r: Value = serde_json::from_str(&last_tool_result(body)).unwrap();
        let (_, reports) = r.as_object().unwrap().iter().next().unwrap();
        text(&[&format!("worker says {}", reports[0]["content"].as_str().unwrap())])
    });
    n.llm.say(WORKER, &["42"]);
    let boss = n.spawn("boss", "delegate").await;
    let m = n.mail().await;
    assert_eq!(m["content"], "worker says 42");
    assert_eq!(m["from"], format!("agent:{boss}"));
    // The worker's prompt came from the boss.
    let wreq = n.llm.requests().into_iter().find(|r| r["messages"][0]["content"] == WORKER).unwrap();
    assert!(wreq["messages"][1]["content"].as_str().unwrap().contains("compute 6*7"));
    let agents = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap();
    let worker = agents.as_array().unwrap().iter().find(|a| a["parent"] == json!(boss)).unwrap();
    assert_eq!(worker["phase"], "idle");
}

#[tokio::test]
async fn hard_pause_keeps_partial_and_resume_continues_it() {
    let n = net().await;
    n.node("s").await;
    n.llm.set_gap(Duration::from_millis(150));
    n.llm.say(WORKER, &["The ", "quick ", "brown ", "fox ", "jumps ", "over"]);
    let id = n.spawn("worker", "write").await;
    n.until(id, "some streamed text", |t| t["partial"]["content"].as_str().is_some_and(|c| c.len() >= 4)).await;
    n.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    let t = n.until(id, "paused", |t| t["paused"] == true).await;
    let partial = t["partial"]["content"].as_str().unwrap().to_string();
    assert!(partial.starts_with("The "), "{partial}");
    assert!(!partial.ends_with("over"), "stream should have been cut: {partial}");

    n.llm.set_gap(Duration::ZERO);
    n.llm.say(WORKER, &["<rest>"]);
    n.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    let m = n.mail().await;
    assert_eq!(m["content"], format!("{partial}<rest>"));
    let req = n.llm.requests().pop().unwrap();
    let msgs = req["messages"].as_array().unwrap();
    assert_eq!(msgs[msgs.len() - 2]["content"], format!("{partial}[interrupted]"));
    assert_eq!(msgs[msgs.len() - 1]["role"], "user");
}

#[tokio::test]
async fn prefill_models_continue_the_partial_directly() {
    let n = Net::new(&cluster("  prefill = true")).await;
    n.node("s").await;
    n.llm.set_gap(Duration::from_millis(150));
    n.llm.say(WORKER, &["a", "b", "c", "d", "e", "f"]);
    let id = n.spawn("worker", "x").await;
    n.until(id, "text", |t| t["partial"]["content"].as_str().is_some_and(|c| c.len() >= 2)).await;
    n.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    let t = n.until(id, "paused", |t| t["paused"] == true).await;
    let partial = t["partial"]["content"].as_str().unwrap().to_string();
    n.llm.set_gap(Duration::ZERO);
    n.llm.say(WORKER, &["Z"]);
    n.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(n.mail().await["content"], format!("{partial}Z"));
    let req = n.llm.requests().pop().unwrap();
    assert_eq!(req["continue_final_message"], true);
    assert_eq!(req["messages"].as_array().unwrap().last().unwrap()["content"], partial);
}

#[tokio::test]
async fn node_crash_moves_agent_and_keeps_partial() {
    let n = net().await;
    let a = n.node("a").await;
    n.llm.set_gap(Duration::from_millis(150));
    n.llm.say(WORKER, &["alpha ", "beta ", "gamma ", "delta ", "epsilon"]);
    let id = n.spawn("worker", "greek").await;
    n.until(id, "on node a", |t| t["node"] == "a").await;
    let t = n.until(id, "partial", |t| t["partial"]["content"].as_str().is_some_and(|c| c.len() >= 6)).await;
    assert!(t["partial"].is_object());

    n.llm.set_gap(Duration::ZERO);
    n.llm.say(WORKER, &["<continued>"]);
    n.hub.disconnect(a).await; // node a dies
    n.node("b").await;
    let m = n.mail().await;
    let c = m["content"].as_str().unwrap();
    assert!(c.starts_with("alpha "), "{c}");
    assert!(c.ends_with("<continued>"), "{c}");
    assert_eq!(n.t(id).await["node"], "b");
}

#[tokio::test]
async fn approval_gates_tool() {
    let n = Net::new(&cluster("  approve = [\"list_agents\"]")).await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "list_agents", json!({})));
    n.llm.push(WORKER, |_| text(&["listed"]));
    let id = n.spawn("worker", "list").await;
    let t = n.until(id, "approval", |t| t["awaiting_approval"].is_object()).await;
    assert_eq!(t["awaiting_approval"]["function"]["name"], "list_agents");
    assert_eq!(t["awaiting_approval"]["id"], "c1");
    assert_eq!(t["paused"], false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(n.llm.requests().len(), 1, "tool must not run before approval");
    n.hub.op(&Addr::root(), Op::Approve { id, call_id: "c1".into(), approved: true }).await.unwrap();
    assert_eq!(n.mail().await["content"], "listed");
    assert_eq!(n.t(id).await["awaiting_approval"], Value::Null);
}

#[tokio::test]
async fn llm_error_fails_agent_and_resume_retries() {
    let n = net().await;
    n.node("s").await;
    // No scripted reply → mock answers 500 → retried, then fails.
    let id = n.spawn("worker", "x").await;
    let m = n.mail().await;
    assert_eq!(m["status"], "failed");
    assert_eq!(n.t(id).await["phase"], "failed");
    n.llm.say(WORKER, &["recovered"]);
    n.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(n.mail().await["content"], "recovered");
}

#[tokio::test]
async fn cancel_stops_streaming_agent() {
    let n = net().await;
    n.node("s").await;
    n.llm.set_gap(Duration::from_millis(200));
    n.llm.say(WORKER, &["a", "b", "c", "d", "e"]);
    let id = n.spawn("worker", "x").await;
    n.until(id, "thinking", |t| t["phase"] == "thinking").await;
    n.hub.op(&Addr::root(), Op::Cancel { id }).await.unwrap();
    assert_eq!(n.mail().await["status"], "cancelled");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t = n.t(id).await;
    assert_eq!(t["phase"], "cancelled");
    assert_eq!(t["node"], Value::Null);
}

#[tokio::test]
async fn agents_message_each_other_and_replies_come_back() {
    let n = net().await;
    n.node("s").await;
    // B (a boss, for its own reply queue) answers; A messages B, then reports what B said.
    n.llm.say(BOSS, &["B idle"]);
    let b = n.spawn("boss", "wait").await;
    assert_eq!(n.mail().await["content"], "B idle");
    n.llm.push(WORKER, move |_| tool_call("m1", "send_message", json!({"to": b.to_string(), "content": "ping"})));
    n.llm.push(WORKER, |_| text(&["sent"]));
    n.llm.say(BOSS, &["pong"]);
    n.llm.push(WORKER, |body| {
        let last = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
        text(&[&format!("A got: {last}")])
    });
    let a = n.spawn("worker", "talk to B").await;
    let mut got = vec![];
    for _ in 0..2 {
        got.push(n.mail().await["content"].as_str().unwrap().to_string());
    }
    assert_eq!(got[0], "sent");
    assert_eq!(got[1], format!("A got: [reply from agent:{b}]\npong"));
    let t = n.t(b).await;
    assert!(t["messages"].to_string().contains(&format!("[message from agent:{a}]")));
}

#[tokio::test]
async fn boss_pauses_and_cancels_its_worker() {
    let n = net().await;
    n.node("s").await;
    n.llm.set_gap(Duration::from_millis(100));
    n.llm.say(WORKER, &["w1 ", "w2 ", "w3 ", "w4 ", "w5 ", "w6 ", "w7 ", "w8"]);
    n.llm.push(BOSS, |_| tool_call("s1", "spawn_agent", json!({"type":"worker","prompt":"long job"})));
    n.llm.push(BOSS, |body| {
        let id = serde_json::from_str::<Value>(&last_tool_result(body)).unwrap()["id"].as_str().unwrap().to_string();
        tool_call("p1", "pause_agent", json!({"id": id, "mode": "hard"}))
    });
    n.llm.push(BOSS, |body| {
        let id = body["messages"].as_array().unwrap().iter().find_map(|m| {
            let c = m["content"].as_str()?;
            serde_json::from_str::<Value>(c).ok()?.get("id")?.as_str().map(String::from)
        });
        tool_call("c1", "cancel_agent", json!({"id": id.unwrap()}))
    });
    n.llm.push(BOSS, |_| text(&["stopped it"]));
    let boss = n.spawn("boss", "start and stop").await;
    let m = n.mail().await;
    assert_eq!(m["content"], "stopped it");
    let agents = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap();
    let w = agents.as_array().unwrap().iter().find(|a| a["parent"] == json!(boss)).unwrap();
    assert_eq!(w["phase"], "cancelled");
    // The boss's tool results confirm both ops.
    let bt = n.t(boss).await.to_string();
    assert!(bt.contains("paused") && bt.contains("cancelled"), "{bt}");
}

#[tokio::test]
async fn agents_cannot_touch_strangers() {
    let n = net().await;
    n.node("s").await;
    n.llm.say(WORKER, &["x"]);
    let stranger = n.spawn("worker", "hi").await;
    n.mail().await;
    n.llm.push(WORKER, move |_| tool_call("c", "cancel_agent", json!({"id": stranger.to_string()})));
    n.llm.push(WORKER, |body| text(&[&last_tool_result(body)]));
    n.spawn("worker", "cancel them").await;
    let c = n.mail().await["content"].as_str().unwrap().to_string();
    assert!(c.contains("not a descendant"), "{c}");
    assert_eq!(n.t(stranger).await["phase"], "idle");
}

const CONCIERGE: &str = "You are the concierge.";

fn resident_cluster() -> String {
    format!(
        "{}{}mixture \"desk\" {{\n  agent = \"clerk\"\n  mailboxes = [\"door\"]\n}}\nresident \"concierge\" {{\n  mixture = \"desk\"\n  prompt = \"start your shift\"\n}}\n",
        nodes(&["s"]),
        agent("clerk", CONCIERGE, "{llm}", &["s"], ""),
    )
}

#[tokio::test]
async fn residents_are_created_addressable_and_removed() {
    let n = Net::new(&resident_cluster()).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    // Created once its node is ready; its first answer goes to whoever applied.
    assert_eq!(n.mail().await["content"], "on duty");
    n.llm.say(CONCIERGE, &["hello yourself"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Resident("concierge".into()), content: "hello".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "hello yourself");
    let agents = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap();
    assert_eq!(agents.as_array().unwrap().len(), 1, "created exactly once");
    let id: uuid::Uuid = agents[0]["id"].as_str().unwrap().parse().unwrap();
    // Dropping it from the cluster cancels it.
    n.apply(&format!("{}{}", nodes(&["s"]), agent("clerk", CONCIERGE, "{llm}", &["s"], ""))).await;
    n.until(id, "cancelled", |t| t["phase"] == "cancelled").await;
    let e = n.hub.op(&Addr::root(), Op::Send { to: Addr::Resident("concierge".into()), content: "?".into() }).await;
    assert!(e.unwrap_err().contains("no resident"));
}

#[tokio::test]
async fn mailboxes_are_readable_only_by_listed_mixtures() {
    let n = Net::new(&resident_cluster()).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    n.mail().await;
    let door = Addr::Mailbox("door".into());
    for k in ["knock", "knock knock"] {
        n.hub.op(&Addr::root(), Op::Send { to: door.clone(), content: k.into() }).await.unwrap();
    }
    n.llm.push(CONCIERGE, |_| tool_call("p", "mailbox_peek", json!({"name":"door"})));
    n.llm.push(CONCIERGE, |_| tool_call("t", "mailbox_take", json!({"name":"door","max":1})));
    n.llm.push(CONCIERGE, |_| tool_call("x", "mailbox_take", json!({"name":"secret"})));
    n.llm.push(CONCIERGE, |body| {
        let tools: Vec<_> = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect();
        text(&[&tools.join("\n")])
    });
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Resident("concierge".into()), content: "check the door".into() }).await.unwrap();
    let c = n.mail().await["content"].as_str().unwrap().to_string();
    let lines: Vec<_> = c.lines().collect();
    let peeked: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(peeked.as_array().unwrap().len(), 2, "peek leaves both");
    let taken: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(taken[0]["content"], "knock");
    assert!(lines[2].contains("not listed"), "{c}");
    // One message is left.
    let left = n.hub.op(&Addr::root(), Op::MailboxPeek { name: "door".into(), max: 10 }).await.unwrap();
    assert_eq!(left[0]["content"], "knock knock");
}
