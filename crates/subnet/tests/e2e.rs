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
async fn a_resident_follows_its_type_to_a_new_version() {
    let n = Net::new(&resident_cluster()).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    assert_eq!(n.mail().await["content"], "on duty");
    let old: uuid::Uuid = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap()[0]["id"].as_str().unwrap().parse().unwrap();
    // A changed type (a new version): no node would resume the old agent,
    // so the resident moves onto the new one with its history.
    let changed = resident_cluster().replace("  model = \"mock\"", "  model = \"mock\"\n  params = { temperature = 0.5 }");
    assert_ne!(changed, resident_cluster());
    n.apply(&changed).await;
    n.until(old, "the old version cancelled", |t| t["phase"] == "cancelled").await;
    n.llm.say(CONCIERGE, &["still here"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Resident("concierge".into()), content: "remember me?".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "still here");
    let msgs = n.llm.requests().pop().unwrap()["messages"].as_array().unwrap().len();
    assert_eq!(msgs, 4, "system, start your shift, on duty, remember me?: the history moved along");
    // A message to the old agent itself (an address resolved before the move) goes on to the copy.
    n.llm.say(CONCIERGE, &["over here"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Agent(old), content: "hello?".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "over here");
    assert_eq!(n.llm.requests().pop().unwrap()["messages"].as_array().unwrap().len(), 6, "in the copy's conversation");
    let agents = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap();
    let new = agents.as_array().unwrap().iter().find(|a| a["id"] != old.to_string()).unwrap();
    assert!(new.get("outdated").is_none(), "{new}");
}

#[tokio::test]
async fn a_cancelled_resident_starts_afresh() {
    let n = Net::new(&resident_cluster()).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    assert_eq!(n.mail().await["content"], "on duty");
    let old: uuid::Uuid = n.hub.op(&Addr::root(), Op::ListAgents).await.unwrap()[0]["id"].as_str().unwrap().parse().unwrap();
    n.llm.say(CONCIERGE, &["back on duty"]);
    n.hub.cancel(&Addr::root(), old).await.unwrap();
    // The cancel's report, then the fresh resident's first answer.
    let mut seen = vec![];
    while seen.len() < 2 {
        seen.push(n.mail().await["content"].as_str().unwrap().to_string());
    }
    assert!(seen.contains(&"back on duty".to_string()), "{seen:?}");
    n.llm.say(CONCIERGE, &["yes?"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Resident("concierge".into()), content: "hello".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "yes?");
    let msgs = n.llm.requests().pop().unwrap()["messages"].as_array().unwrap().len();
    assert_eq!(msgs, 4, "a fresh conversation: system, start your shift, back on duty, hello");
}

#[tokio::test]
async fn an_outdated_agent_is_upgraded() {
    let n = net().await;
    n.node("a").await;
    n.llm.say(WORKER, &["first"]);
    let id = n.spawn("worker", "one").await;
    assert_eq!(n.mail().await["content"], "first");
    n.apply(&cluster("  params = { temperature = 0.5 }")).await;
    n.until(id, "listed as outdated", |t| t["outdated"] == true).await;
    // Until the node offers the new version, there's nothing to move onto.
    let mut up = n.hub.upgrade(&Addr::root(), id, false).await;
    for _ in 0..200 {
        match &up {
            Err(e) if e.to_string().contains("no live node") => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                up = n.hub.upgrade(&Addr::root(), id, false).await;
            }
            _ => break,
        }
    }
    let up = up.unwrap();
    assert_ne!(up.id, id);
    n.until(id, "the old one cancelled", |t| t["phase"] == "cancelled").await;
    n.llm.say(WORKER, &["second"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Agent(up.id), content: "two".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "second");
    let msgs = n.llm.requests().pop().unwrap()["messages"].as_array().unwrap().len();
    assert_eq!(msgs, 4, "system, one, first, two");
    // Agents may not move agents; children are moved from their root.
    assert!(n.hub.upgrade(&Addr::Agent(up.id), up.id, false).await.is_err());
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

#[tokio::test]
async fn nodes_resume_agents_from_snapshots() {
    let n = net().await;
    n.hub.set_snapshot_every(2);
    let a = n.node("a").await;
    n.llm.say(WORKER, &["first"]);
    let id = n.spawn("worker", "one").await;
    assert_eq!(n.mail().await["content"], "first");
    n.hub.disconnect(a).await;
    n.node("b").await;
    n.llm.say(WORKER, &["second"]);
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Agent(id), content: "two".into() }).await.unwrap();
    assert_eq!(n.mail().await["content"], "second");
    let msgs = n.llm.requests().pop().unwrap()["messages"].as_array().unwrap().len();
    assert_eq!(msgs, 4, "system, one, first, two: the history survived the snapshot");
}

#[tokio::test]
async fn long_conversations_are_compacted() {
    let n = Net::new(&cluster("  compact = { at_tokens = 50, keep = 2 }")).await;
    n.node("s").await;
    // Two tool calls, each reporting a context over the threshold.
    let call = |id: &'static str| {
        move |_: &Value| {
            let mut c = tool_call(id, "list_types", json!({}));
            c[1] = json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":80,"completion_tokens":5}}).to_string();
            c
        }
    };
    n.llm.push(WORKER, call("c1"));
    n.llm.push(WORKER, call("c2"));
    n.llm.say(subnet_core::agent::COMPACT_PROMPT, &["found: boss and worker"]);
    n.llm.push(WORKER, |body| text(&[&body["messages"].as_array().unwrap().len().to_string()]));
    let id = n.spawn("worker", "look around").await;
    // system, task, summary, then the kept call and its result.
    assert_eq!(n.mail().await["content"], "5");
    let reqs = n.llm.requests();
    let summarise = reqs.iter().find(|r| r["messages"][0]["content"] == subnet_core::agent::COMPACT_PROMPT).expect("a summary was asked for");
    assert!(summarise.get("tools").is_none_or(|t| t.as_array().is_none_or(Vec::is_empty)), "no tools for the summary");
    assert!(summarise["messages"][1]["content"].as_str().unwrap().contains("look around"));
    let last = reqs.last().unwrap();
    assert_eq!(last["messages"][1]["content"], "look around", "the task stays");
    assert!(last["messages"][2]["content"].as_str().unwrap().contains("found: boss and worker"));
    let t = n.t(id).await;
    assert_eq!(t["compactions"], 1);
    // Nothing's lost: the full transcript has what was compacted away, and
    // where the compaction was.
    let full = serde_json::to_value(n.hub.transcript_of(id, true).await.unwrap()).unwrap();
    let msgs = full["messages"].as_array().unwrap();
    assert!(msgs.len() > t["messages"].as_array().unwrap().len(), "{full}");
    assert!(msgs.iter().any(|m| m["tool_call_id"] == "c1"), "the first call's result, compacted away");
    assert!(!msgs.iter().any(|m| m["content"].as_str().is_some_and(|c| c.contains("[The conversation so far was compacted"))));
    assert_eq!(full["compacted"][0]["summary"], "found: boss and worker");
    assert!(full["compacted"][0]["from"].as_u64().unwrap() >= 1);
    // A watcher following the full transcript sees it too.
    let w = n.hub.watch(subnet::api::WatchArgs { id, after: Some(0), tail: None, timeout_ms: Some(0), max_chars: None, full: true }).await.unwrap();
    assert_eq!(w.next as usize, msgs.len());
    assert_eq!(w.compacted.len(), 1);
}

#[tokio::test]
async fn compaction_can_be_asked_for() {
    let n = Net::new(&cluster("  compact = { at_tokens = 1000000, keep = 2 }")).await; // the threshold far away
    n.node("s").await;
    for i in 0..5 {
        n.llm.push(WORKER, move |_| text(&[&format!("answer {i}")]));
    }
    let id = n.spawn("worker", "look around").await;
    n.mail().await;
    for i in 1..5 {
        n.hub.send(&Addr::root(), Addr::Agent(id), format!("and {i}?")).await.unwrap();
        n.mail().await;
    }
    n.llm.say(subnet_core::agent::COMPACT_PROMPT, &["we talked five times"]);
    let calls = n.llm.requests().len();
    n.hub.compact(&Addr::root(), id).await.unwrap();
    let t = n.until(id, "compacted", |t| t["compactions"] == 1 && t["phase"] == "idle").await;
    assert!(t["messages"].as_array().unwrap().iter().any(|m| m["content"].as_str().is_some_and(|c| c.contains("we talked five times"))));
    assert_eq!(n.llm.requests().len(), calls + 1, "only the summary: no turn after it");
    // Nothing lost.
    let full = n.hub.transcript_of(id, true).await.unwrap();
    assert!(full.messages.iter().any(|m| m.content.as_deref() == Some("answer 0")));
}

#[tokio::test]
async fn a_failed_model_call_is_resumed_and_says_why() {
    let n = Net::new(&cluster("")).await;
    n.node("s").await;
    // No scripted reply: the model answers 500 until it gives up.
    let id = n.spawn("worker", "look around").await;
    let t = n.until(id, "failed", |t| t["phase"] == "failed").await;
    assert!(t["error"].as_str().is_some_and(|e| e.contains("no scripted reply")), "{t}");
    assert!(n.mail().await["content"].as_str().unwrap().contains("no scripted reply"), "the failure is reported");
    // The model is back: resume goes on from there.
    n.llm.push(WORKER, |_| text(&["looked"]));
    n.hub.resume(&Addr::root(), id, false).await.unwrap();
    assert_eq!(n.mail().await["content"], "looked");
    let t = n.t(id).await;
    assert!(t["error"].is_null() && t["phase"] == "idle");
}

#[tokio::test]
async fn an_agent_searches_what_was_summarised_away() {
    let n = Net::new(&cluster("  search_history = true\n  compact = { at_tokens = 1000000, keep = 2 }")).await;
    n.node("s").await;
    n.llm.push(WORKER, |_| text(&["noted"]));
    let id = n.spawn("worker", "remember: the door code is Swordfish-42").await;
    n.mail().await;
    for i in 1..4 {
        n.llm.push(WORKER, move |_| text(&[&format!("answer {i}")]));
        n.hub.send(&Addr::root(), Addr::Agent(id), format!("and {i}?")).await.unwrap();
        n.mail().await;
    }
    n.llm.say(subnet_core::agent::COMPACT_PROMPT, &["we talked a while"]);
    n.hub.compact(&Addr::root(), id).await.unwrap();
    n.until(id, "compacted", |t| t["compactions"] == 1 && t["phase"] == "idle").await;
    // The code is out of its context now; it searches and finds it.
    n.llm.push(WORKER, |_| tool_call("c1", "search_history", json!({"pattern": "door code is \\w+"})));
    n.llm.push(WORKER, |body| {
        let r: Value = serde_json::from_str(&last_tool_result(body)).unwrap();
        assert_eq!(r["matches"], 1, "{r}");
        assert_eq!(r["hits"][0]["summarised"], false, "the task stays: {r}");
        text(&["it's Swordfish-42"])
    });
    n.hub.send(&Addr::root(), Addr::Agent(id), "what was the code?".into()).await.unwrap();
    assert_eq!(n.mail().await["content"], "it's Swordfish-42");
    // An answer from before the summary is marked so; its own searches aren't hits.
    n.llm.push(WORKER, |_| tool_call("c2", "search_history", json!({"pattern": "answer 1|search_history"})));
    n.llm.push(WORKER, |body| {
        let r: Value = serde_json::from_str(&last_tool_result(body)).unwrap();
        assert_eq!((r["matches"].as_u64(), r["hits"][0]["summarised"].as_bool(), r["hits"][0]["text"].as_str()), (Some(1), Some(true), Some("answer 1")), "{r}");
        text(&["yes"])
    });
    n.hub.send(&Addr::root(), Addr::Agent(id), "and the first answer?".into()).await.unwrap();
    assert_eq!(n.mail().await["content"], "yes");
    n.llm.push(WORKER, |_| tool_call("c3", "search_history", json!({"pattern": "("})));
    n.llm.push(WORKER, |body| {
        assert!(last_tool_result(body).contains("bad pattern"));
        text(&["ok"])
    });
    n.hub.send(&Addr::root(), Addr::Agent(id), "try a broken one".into()).await.unwrap();
    assert_eq!(n.mail().await["content"], "ok");
}

#[tokio::test]
async fn a_long_tool_result_reaches_it_cut_and_it_greps_the_rest() {
    let n = Net::new(&format!("{}mixture \"picker\" {{\n  agent = \"worker\"\n  mailboxes = [\"box\"]\n}}\n", cluster("  grep_results = 600"))).await;
    n.node("s").await;
    // A long message in a mailbox: what it peeks at is long.
    let long: String = (1..=400).map(|i| if i == 321 { "line 321: the needle is here\n".to_string() } else { format!("line {i}: hay\n") }).collect();
    n.hub.send(&Addr::root(), Addr::Mailbox("box".into()), long).await.unwrap();
    n.llm.push(WORKER, |body| {
        assert!(body["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "grep_result"));
        tool_call("c1", "mailbox_peek", json!({"name": "box"}))
    });
    n.llm.push(WORKER, |body| {
        let seen = last_tool_result(body);
        assert!(seen.chars().count() < 800 && !seen.contains("needle"), "cut: {seen}");
        assert!(seen.contains("grep_result(call: \"c1\""), "{seen}");
        assert!(body["messages"].as_array().unwrap().iter().all(|m| m.get("full").is_none()), "the whole isn't sent");
        tool_call("c2", "grep_result", json!({"call": "c1", "pattern": "NEEDLE"}))
    });
    n.llm.push(WORKER, |body| {
        let r: Value = serde_json::from_str(&last_tool_result(body)).unwrap();
        assert_eq!(r["matches"], 1, "{r}");
        assert!(r["hits"][0]["lines"].as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap().contains("the needle is here")), "{r}");
        tool_call("c3", "grep_result", json!({"call": "nope", "pattern": "x"}))
    });
    n.llm.push(WORKER, |body| {
        assert!(last_tool_result(body).contains("no result of a tool call"), "{}", last_tool_result(body));
        text(&["found it"])
    });
    let id = n.spawn("picker", "what's in the box?").await;
    assert_eq!(n.mail().await["content"], "found it");
    // The transcript keeps the whole.
    let t = n.hub.transcript_of(id, true).await.unwrap();
    assert!(t.messages.iter().any(|m| m.full.as_deref().is_some_and(|f| f.contains("needle"))));
}

#[tokio::test]
async fn without_search_history_there_is_no_such_tool() {
    let n = Net::new(&cluster("")).await;
    n.node("s").await;
    n.llm.push(WORKER, |body| {
        assert!(!body["tools"].as_array().unwrap().iter().any(|t| matches!(t["function"]["name"].as_str(), Some("search_history" | "grep_result"))));
        text(&["fine"])
    });
    n.spawn("worker", "hi").await;
    assert_eq!(n.mail().await["content"], "fine");
}
