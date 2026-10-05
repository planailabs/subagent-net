//! Hooks end to end: a hub running them at a URL, as an agent, timing out,
//! and settled by hand; their answers steering the agent's loop.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::llm::{last_tool_result, text, tool_call};
use common::net::{Net, agent, nodes};
use serde_json::{Value, json};
use subnet_core::addr::Addr;

const WORKER: &str = "You are a worker.";
const JUDGE: &str = "You judge.";

/// A hook server: answers each question with `decide(question)`, and keeps
/// the questions.
async fn hook_server(decide: impl Fn(&Value) -> Value + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(vec![]));
    let (s, decide) = (seen.clone(), Arc::new(decide));
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(q): axum::Json<Value>| {
            let (s, decide) = (s.clone(), decide.clone());
            async move {
                s.lock().unwrap().push(q.clone());
                axum::Json(decide(&q))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

/// A worker type on node s with these hooks (blocks given whole).
fn cluster(hooks: &str, names: &[&str]) -> String {
    let list = names.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ");
    format!(
        "{}{}{}{hooks}",
        nodes(&["s"]),
        agent("worker", WORKER, "{llm}", &["s"], &format!("  hooks = [{list}]")),
        agent("judge", JUDGE, "{llm}", &["s"], ""),
    )
}

#[tokio::test]
async fn a_url_hook_denies_a_call_continues_a_turn_and_signs_the_answer() {
    let (url, seen) = hook_server(|q| match q["point"].as_str().unwrap() {
        "pre_tool" if q["input"]["tool"] == "list_agents" => json!({"decision": "deny", "reason": "not your business"}),
        "on_turn_end" if !q["input"]["content"].as_str().unwrap().contains("checked") => json!({"decision": "continue", "text": "check your work"}),
        "on_report" => json!({"decision": "rewrite", "text": format!("{} (signed)", q["input"]["content"].as_str().unwrap())}),
        _ => json!({"decision": "allow"}),
    })
    .await;
    let hooks = format!(
        "hook \"policy\" {{\n  on = \"pre_tool\"\n  tools = [\"list_*\"]\n  run {{ url = {url:?} }}\n}}\nhook \"review\" {{\n  on = \"on_turn_end\"\n  run {{ url = {url:?} }}\n}}\nhook \"sign\" {{\n  on = \"on_report\"\n  run {{ url = {url:?} }}\n}}\n"
    );
    let n = Net::new(&cluster(&hooks, &["policy", "review", "sign"])).await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "list_agents", json!({})));
    n.llm.push(WORKER, |body| {
        assert_eq!(last_tool_result(body), "[denied by hook policy: not your business]");
        text(&["done"])
    });
    n.llm.push(WORKER, |body| {
        let last = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
        assert_eq!(last, "[from hook review]\ncheck your work");
        text(&["done, checked"])
    });
    n.spawn("worker", "go").await;
    assert_eq!(n.mail().await["content"], "done, checked (signed)");
    let points: Vec<String> = seen.lock().unwrap().iter().map(|q| q["point"].as_str().unwrap().to_string()).collect();
    assert_eq!(points, ["pre_tool", "on_turn_end", "on_turn_end", "on_report"]);
    let q = &seen.lock().unwrap()[0];
    assert_eq!((q["hook"].as_str(), q["agent"]["type"].as_str().map(|t| t.starts_with("worker@"))), (Some("policy"), Some(true)));
}

#[tokio::test]
async fn a_pre_model_hook_injects_context_the_model_reads() {
    let (url, seen) = hook_server(|q| match q["point"].as_str().unwrap() {
        "pre_model" => json!({"decision": "allow", "inject": [format!("The time is 09:00 (call {}).", q["input"]["messages"])]}),
        _ => json!({"decision": "allow"}),
    })
    .await;
    let hooks = format!("hook \"clock\" {{\n  on = \"pre_model\"\n  run {{ url = {url:?} }}\n}}\n");
    let n = Net::new(&cluster(&hooks, &["clock"])).await;
    n.node("s").await;
    n.llm.push(WORKER, |body| {
        let last = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
        assert!(last.starts_with("[from hook clock]\nThe time is 09:00"), "{last}");
        text(&["it's nine"])
    });
    n.spawn("worker", "what time is it?").await;
    assert_eq!(n.mail().await["content"], "it's nine");
    assert_eq!(seen.lock().unwrap().iter().filter(|q| q["point"] == "pre_model").count(), 1);
}

#[tokio::test]
async fn when_false_lets_it_through_without_asking() {
    let (url, seen) = hook_server(|_| json!({"decision": "deny"})).await;
    let hooks = format!("hook \"policy\" {{\n  on = \"pre_tool\"\n  when = \"input.args.size() > 0\"\n  run {{ url = {url:?} }}\n}}\n");
    let n = Net::new(&cluster(&hooks, &["policy"])).await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "list_types", json!({})));
    n.llm.push(WORKER, |body| {
        assert!(!last_tool_result(body).contains("denied"), "ran: {}", last_tool_result(body));
        text(&["ok"])
    });
    n.spawn("worker", "go").await;
    assert_eq!(n.mail().await["content"], "ok");
    assert!(seen.lock().unwrap().is_empty(), "never asked");
    // A `when` that isn't CEL is refused when applied.
    let bad = cluster("hook \"h\" {\n  on = \"pre_tool\"\n  when = \"((\"\n  run { url = \"http://x\" }\n}\n", &["h"]).replace("{llm}", &n.llm.url);
    let e = n.hub.apply_cluster(vec![subnet::hub::db::ClusterFile { name: "c.hcl".into(), text: bad }], false, &Addr::root()).await.unwrap_err();
    assert!(e.to_string().contains("hook \"h\": when"), "{e}");
}

#[tokio::test]
async fn an_agent_decides_and_a_silent_hook_falls_back_to_on_lost() {
    // A judge (another agent) denies; a URL that never answers times out: on_lost allow.
    let silent = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/never", l.local_addr().unwrap());
        let app = axum::Router::new().route("/never", axum::routing::post(|| async { tokio::time::sleep(Duration::from_secs(60)).await }));
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    };
    let hooks = format!(
        "hook \"judge\" {{\n  on = \"pre_tool\"\n  tools = [\"list_agents\"]\n  run {{ spawn = \"judge\" }}\n}}\nhook \"slow\" {{\n  on = \"pre_tool\"\n  tools = [\"list_types\"]\n  run {{ url = {silent:?} }}\n  timeout = \"300ms\"\n  on_lost = \"allow\"\n}}\n"
    );
    let n = Net::new(&cluster(&hooks, &["judge", "slow"])).await;
    n.node("s").await;
    n.llm.push(JUDGE, |body| {
        let q = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
        assert!(q.contains("list_agents") && q.contains("\"deny\""), "{q}");
        text(&["Here: {\"decision\": \"deny\", \"reason\": \"the judge says no\"}"])
    });
    n.llm.push(WORKER, |_| common::llm::tool_calls(&[("c1", "list_agents", json!({})), ("c2", "list_types", json!({}))]));
    n.llm.push(WORKER, |body| {
        let tools: Vec<String> = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap().to_string()).collect();
        assert_eq!(tools[0], "[denied by hook judge: the judge says no]");
        assert!(!tools[1].contains("denied"), "the slow one let it through: {}", tools[1]);
        text(&["both judged"])
    });
    n.spawn("worker", "go").await;
    assert_eq!(n.mail().await["content"], "both judged");
}

#[tokio::test]
async fn a_person_settles_a_waiting_hook() {
    // A hook that never answers, with a long timeout: someone decides by hand.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/never", l.local_addr().unwrap());
    let app = axum::Router::new().route("/never", axum::routing::post(|| async { tokio::time::sleep(Duration::from_secs(600)).await }));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let hooks = format!("hook \"gate\" {{\n  on = \"pre_tool\"\n  run {{ url = {url:?} }}\n  timeout = \"10m\"\n}}\n");
    let n = Net::new(&cluster(&hooks, &["gate"])).await;
    n.node("s").await;
    n.llm.push(WORKER, |_| tool_call("c1", "list_types", json!({})));
    n.llm.push(WORKER, |body| {
        assert_eq!(last_tool_result(body), "[denied by hook gate: no thanks]");
        text(&["fine"])
    });
    let id = n.spawn("worker", "go").await;
    let t = n.until(id, "waiting for its hook", |t| t["hooks"].as_array().is_some_and(|h| !h.is_empty())).await;
    assert_eq!((t["hooks"][0]["name"].as_str(), t["hooks"][0]["run"].as_str()), (Some("gate"), Some("h1.0")));
    // Only a run it waits for.
    assert!(n.hub.settle_hook(id, "h9.0", subnet_core::hooks::Outcome::allow()).await.is_err());
    let deny = subnet_core::hooks::Outcome { decision: subnet_core::hooks::Decision::Deny, reason: Some("no thanks".into()), args: None, text: None, note: None, inject: vec![] };
    n.hub.settle_hook(id, "h1.0", deny).await.unwrap();
    assert_eq!(n.mail().await["content"], "fine");
}

#[tokio::test]
async fn a_hook_running_when_the_hub_stops_is_settled_by_on_lost_after() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/never", l.local_addr().unwrap());
    let app = axum::Router::new().route("/never", axum::routing::post(|| async { tokio::time::sleep(Duration::from_secs(600)).await }));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let hooks = format!("hook \"gate\" {{\n  on = \"pre_tool\"\n  run {{ url = {url:?} }}\n  timeout = \"10m\"\n  on_lost = \"allow\"\n}}\n");
    let llm = common::llm::MockLlm::start().await;
    let text_of = cluster(&hooks, &["gate"]).replace("{llm}", &llm.url);
    let db = common::db_url().await;
    let start = || async {
        let hub = subnet::hub::Hub::open(&db, None).await.unwrap();
        hub.wait_leader().await;
        hub.apply_cluster(vec![subnet::hub::db::ClusterFile { name: "c.hcl".into(), text: text_of.clone() }], false, &Addr::root()).await.unwrap();
        hub
    };
    let hub = start().await;
    common::net::node_ready(&hub, "s").await;
    llm.push(WORKER, |_| tool_call("c1", "list_types", json!({})));
    let id = common::id_of(&hub.op(&Addr::root(), subnet_core::proto::Op::Spawn { ty: "worker".into(), prompt: "go".into(), tenant: None }).await.unwrap());
    for _ in 0..200 {
        if hub.list_agents().await.iter().any(|a| a.id == id && !a.hooks.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    hub.shutdown();
    drop(hub);
    // A new hub: the run was lost, and it isn't idempotent: on_lost allows the call.
    llm.push(WORKER, |body| {
        assert!(!last_tool_result(body).contains("denied"), "{}", last_tool_result(body));
        text(&["went on"])
    });
    let hub = start().await;
    common::net::node_ready(&hub, "s").await;
    for _ in 0..400 {
        if hub.list_agents().await.iter().any(|a| a.id == id && a.last.as_deref() == Some("went on")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the agent never went on: {:?}", hub.list_agents().await.into_iter().find(|a| a.id == id));
}

#[tokio::test]
async fn an_agent_upgraded_while_a_hook_runs_doesnt_wait_for_it_forever() {
    // A hook that never answers (within the test), not idempotent: on_lost allows.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/never", l.local_addr().unwrap());
    let app = axum::Router::new().route("/never", axum::routing::post(|| async { tokio::time::sleep(Duration::from_secs(600)).await }));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let hooks = format!("hook \"slow\" {{\n  on = \"pre_model\"\n  run {{ url = {url:?} }}\n  timeout = \"10m\"\n  on_lost = \"allow\"\n}}\n");
    let n = Net::new(&cluster(&hooks, &["slow"])).await;
    n.node("s").await;
    let id = n.spawn("worker", "go").await;
    n.until(id, "waiting for its hook", |t| t["hooks"].as_array().is_some_and(|h| !h.is_empty())).await;
    // A new version of its type (another prompt), and the agent moved onto it.
    let newer = cluster(&hooks, &["slow"]).replace(WORKER, "You are a worker, now upgraded.").replace("{llm}", &n.llm.url);
    n.hub.apply_cluster(vec![subnet::hub::db::ClusterFile { name: "c.hcl".into(), text: newer }], false, &Addr::root()).await.unwrap();
    n.llm.push("You are a worker, now upgraded.", |_| text(&["went on"]));
    let copy = n.hub.upgrade(&Addr::root(), id, false).await.unwrap();
    // The copy's own hook run was lost with the move: on_lost lets the call go on.
    assert_eq!(n.mail().await["content"], "went on");
    assert!(n.hub.list_agents().await.iter().any(|a| a.id == copy.id && a.hooks.is_empty()));
}

