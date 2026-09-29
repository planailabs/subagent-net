//! User surfaces over real HTTP: the hub's MCP server and event stream.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::db_url;
use common::llm::MockLlm;
use serde_json::{Value, json};
use subnet::client::{Remote, tail};
use subnet::hub::{Hub, http};
use subnet::spawner::config::{Config, TypeConfig};
use subnet::spawner::{Spawner, attach};
use subnet_core::agent::Budget;
use subnet_llm::ModelConfig;

const SYS: &str = "helper";

async fn setup(token: Option<&str>) -> (String, MockLlm) {
    let llm = MockLlm::start().await;
    let hub = Hub::open(&db_url().await, token.map(Into::into)).await.unwrap();
    let t = TypeConfig {
        name: "helper".into(),
        description: "helps".into(),
        system: SYS.into(),
        model: ModelConfig {
            base_url: llm.url.clone(),
            model: "m".into(),
            api_key_env: None,
            prefill: false,
            params: Default::default(),
        },
        mcp: vec![],
        spawns: vec![],
        budget: Budget::default(),
        approve: vec![],
        idempotent: vec![],
    };
    let cfg = Config { hub: String::new(), name: "s".into(), capacity: 4, token_env: None, types: vec![t] };
    attach(hub.clone(), Arc::new(Spawner::new(&cfg).await.unwrap())).await.unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, http::router(hub)).await.unwrap() });
    (base, llm)
}

#[tokio::test]
async fn mcp_client_spawns_and_gets_the_answer_in_its_own_inbox() {
    let (base, llm) = setup(None).await;
    llm.say(SYS, &["at your service"]);
    let c = Remote::connect(&base, None, "claude").await.unwrap();
    let types = c.call("list_types", Value::Null).await.unwrap();
    assert_eq!(types[0]["name"], "helper");
    let spawned = c.call("spawn", json!({"type":"helper","prompt":"hello"})).await.unwrap();
    let id = spawned["id"].as_str().unwrap().to_string();
    let mail = c.call("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    assert_eq!(mail[0]["content"], "at your service");
    assert_eq!(mail[0]["from"], format!("agent:{id}"));
    // The agent saw who asked.
    let t = c.call("transcript", json!({"id": id})).await.unwrap();
    assert!(t["messages"][0]["content"].as_str().unwrap().starts_with("[message from client:claude]"));
    // The user's inbox stays empty: the answer went to the client that asked.
    let u = Remote::connect(&base, None, "user").await.unwrap();
    assert_eq!(u.call("wait_inbox", json!({"timeout_ms": 50})).await.unwrap(), json!([]));
}

#[tokio::test]
async fn control_tools_work_and_errors_surface() {
    let (base, llm) = setup(None).await;
    llm.say(SYS, &["ok"]);
    let u = Remote::connect(&base, None, "user").await.unwrap();
    let id = u.call("spawn", json!({"type":"helper","prompt":"x"})).await.unwrap()["id"].as_str().unwrap().to_string();
    u.call("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    u.call("pause", json!({"id": id, "mode": "safe"})).await.unwrap();
    let agents = u.call("list_agents", Value::Null).await.unwrap();
    assert_eq!(agents[0]["pause"], "safe");
    u.call("resume", json!({"id": id})).await.unwrap();
    let f = u.call("fork", json!({"id": id, "at": 1})).await.unwrap();
    assert!(f["id"].is_string());
    u.call("cancel", json!({"id": id})).await.unwrap();
    let e = u.call("spawn", json!({"type":"nope","prompt":"x"})).await.unwrap_err();
    assert!(e.to_string().contains("no live spawner"), "{e}");
    let e = u.call("pause", json!({"id": "not-a-uuid", "mode": "hard"})).await.unwrap_err();
    assert!(e.to_string().contains("bad agent id"), "{e}");
}

#[tokio::test]
async fn token_is_required_when_configured() {
    let (base, _llm) = setup(Some("sekrit")).await;
    assert!(Remote::connect(&base, None, "user").await.is_err());
    assert!(Remote::connect(&base, Some("wrong"), "user").await.is_err());
    let ok = Remote::connect(&base, Some("sekrit"), "user").await.unwrap();
    ok.call("list_types", Value::Null).await.unwrap();
    assert!(tail(&base, Some("wrong"), None, |_| {}).await.is_err());
}

#[tokio::test]
async fn tail_streams_committed_events() {
    let (base, llm) = setup(None).await;
    llm.say(SYS, &["streamed ", "answer"]);
    let seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    let b2 = base.clone();
    tokio::spawn(async move { tail(&b2, None, None, move |v| s2.lock().unwrap().push(v)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let u = Remote::connect(&base, None, "user").await.unwrap();
    u.call("spawn", json!({"type":"helper","prompt":"x"})).await.unwrap();
    u.call("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let kinds: Vec<String> =
        seen.lock().unwrap().iter().map(|v| v["event"]["type"].as_str().unwrap_or_default().to_string()).collect();
    assert_eq!(kinds.first().map(String::as_str), Some("inbox"));
    assert!(kinds.contains(&"llm_delta".to_string()));
    assert_eq!(kinds.last().map(String::as_str), Some("llm_done"));
}

#[tokio::test]
async fn tail_filters_by_agent() {
    let (base, llm) = setup(None).await;
    llm.say(SYS, &["a"]);
    llm.say(SYS, &["b"]);
    let u = Remote::connect(&base, None, "user").await.unwrap();
    let first =
        u.call("spawn", json!({"type":"helper","prompt":"1"})).await.unwrap()["id"].as_str().unwrap().to_string();
    u.call("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    let b2 = base.clone();
    let agent = first.parse().unwrap();
    tokio::spawn(async move { tail(&b2, None, Some(agent), move |v| s2.lock().unwrap().push(v)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    u.call("spawn", json!({"type":"helper","prompt":"2"})).await.unwrap();
    u.call("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(seen.lock().unwrap().is_empty(), "events of other agents leaked: {:?}", seen.lock().unwrap());
}
