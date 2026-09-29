//! User surfaces over real HTTP: the hub's MCP server and event stream.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::db_url;
use common::llm::MockLlm;
use serde_json::{Value, json};
use subnet::client::{Client, tail};
use subnet::hub::{Hub, http};

const SYS: &str = "helper";

const PRINCIPALS: &str = r#"
client "claude" { role = "operator" }
user "watcher" { role = "viewer" }
node "s" {}
"#;

/// Hub with principals `client:claude` (operator) and `user:watcher` (viewer).
async fn setup(token: Option<&str>) -> (String, MockLlm) {
    let (base, llm, _) = setup_hub(token).await;
    (base, llm)
}

async fn setup_hub(token: Option<&str>) -> (String, MockLlm, Arc<Hub>) {
    let llm = MockLlm::start().await;
    let hub = Hub::open(&db_url().await, token.map(Into::into)).await.unwrap();
    let text = format!("{PRINCIPALS}{}", common::net::agent("helper", SYS, &llm.url, &["s"], "  description = \"helps\""));
    let files = vec![subnet::hub::db::ClusterFile { name: "p.hcl".into(), text }];
    hub.apply_cluster(files, false, &subnet_core::addr::Addr::root()).await.unwrap();
    common::net::node_ready(&hub, "s").await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let h = hub.clone();
    tokio::spawn(async move { axum::serve(l, http::router(h)).await.unwrap() });
    (base, llm, hub)
}

async fn token(hub: &Hub, kind: &str, name: &str) -> String {
    let kind = serde_json::from_value(json!(kind)).unwrap();
    hub.issue_token(kind, name).await.unwrap()
}

#[tokio::test]
async fn mcp_client_spawns_and_gets_the_answer_in_its_own_inbox() {
    let (base, llm, hub) = setup_hub(None).await;
    llm.say(SYS, &["at your service"]);
    let claude = token(&hub, "client", "claude").await;
    let c = remote(&base, Some(&claude), "claude").await.unwrap();
    assert_eq!(c.call_raw("whoami", Value::Null).await.unwrap(), json!({"addr":"client:claude","role":"operator"}));
    let types = c.call_raw("list_types", Value::Null).await.unwrap();
    assert_eq!(types[0]["name"], "helper");
    let spawned = c.call_raw("spawn", json!({"type":"helper","prompt":"hello"})).await.unwrap();
    let id = spawned["id"].as_str().unwrap().to_string();
    let mail = c.call_raw("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    assert_eq!(mail[0]["content"], "at your service");
    assert_eq!(mail[0]["from"], format!("agent:{id}"));
    // The agent saw who asked.
    let t = c.call_raw("transcript", json!({"id": id})).await.unwrap();
    assert!(t["messages"][0]["content"].as_str().unwrap().starts_with("[message from client:claude]"));
    // The user's inbox stays empty: the answer went to the client that asked.
    let u = remote(&base, None, "user").await.unwrap();
    assert_eq!(u.call_raw("wait_inbox", json!({"timeout_ms": 50})).await.unwrap(), json!([]));
}

#[tokio::test]
async fn control_tools_work_and_errors_surface() {
    let (base, llm) = setup(None).await;
    llm.say(SYS, &["ok"]);
    let u = remote(&base, None, "user").await.unwrap();
    let id = u.call_raw("spawn", json!({"type":"helper","prompt":"x"})).await.unwrap()["id"].as_str().unwrap().to_string();
    u.call_raw("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    u.call_raw("pause", json!({"id": id, "mode": "safe"})).await.unwrap();
    let agents = u.call_raw("list_agents", Value::Null).await.unwrap();
    assert_eq!(agents[0]["pause"], "safe");
    u.call_raw("resume", json!({"id": id})).await.unwrap();
    let f = u.call_raw("fork", json!({"id": id, "at": 1})).await.unwrap();
    assert!(f["id"].is_string());
    u.call_raw("cancel", json!({"id": id})).await.unwrap();
    let e = u.call_raw("spawn", json!({"type":"nope","prompt":"x"})).await.unwrap_err();
    assert_eq!(e.kind, subnet_ops::ErrorKind::NotFound, "{e}");
    let e = u.call_raw("pause", json!({"id": "not-a-uuid", "mode": "hard"})).await.unwrap_err();
    assert_eq!(e.kind, subnet_ops::ErrorKind::BadRequest, "{e}");
}

#[tokio::test]
async fn token_is_required_when_configured() {
    let (base, _llm) = setup(Some("sekrit")).await;
    assert!(remote(&base, None, "user").await.is_err());
    assert!(remote(&base, Some("wrong"), "user").await.is_err());
    let ok = remote(&base, Some("sekrit"), "user").await.unwrap();
    ok.call_raw("list_types", Value::Null).await.unwrap();
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
    let u = remote(&base, None, "user").await.unwrap();
    u.call_raw("spawn", json!({"type":"helper","prompt":"x"})).await.unwrap();
    u.call_raw("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
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
    let u = remote(&base, None, "user").await.unwrap();
    let first =
        u.call_raw("spawn", json!({"type":"helper","prompt":"1"})).await.unwrap()["id"].as_str().unwrap().to_string();
    u.call_raw("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    let b2 = base.clone();
    let agent = first.parse().unwrap();
    tokio::spawn(async move { tail(&b2, None, Some(agent), move |v| s2.lock().unwrap().push(v)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    u.call_raw("spawn", json!({"type":"helper","prompt":"2"})).await.unwrap();
    u.call_raw("wait_inbox", json!({"timeout_ms": 5000})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(seen.lock().unwrap().is_empty(), "events of other agents leaked: {:?}", seen.lock().unwrap());
}

async fn remote(base: &str, token: Option<&str>, who: &str) -> Result<Client, subnet_ops::OpError> {
    let _ = who;
    let c = Client::new(base, token.map(String::from));
    // Fail early like a session handshake would.
    c.call_raw("list_types", serde_json::Value::Null).await?;
    Ok(c)
}

#[tokio::test]
async fn hub_mcp_endpoint_serves_the_same_operations() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    let (base, llm, hub) = setup_hub(Some("adm")).await;
    llm.say(SYS, &["via mcp"]);
    let claude = token(&hub, "client", "claude").await;
    let cfg = StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(claude);
    let mcp = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap();
    let tools: Vec<String> = mcp.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    for t in ["spawn", "send", "wait_inbox", "pause", "resume", "cancel", "approve", "fork", "transcript", "list_agents", "list_types"] {
        assert!(tools.contains(&t.to_string()), "missing {t}: {tools:?}");
    }
    assert!(!tools.contains(&"apply_cluster".to_string()), "an operator sees no admin tools");
    let mut p = CallToolRequestParams::new("spawn");
    p.arguments = json!({"type":"helper","prompt":"hi"}).as_object().cloned();
    let r = mcp.call_tool(p).await.unwrap();
    assert_eq!(r.is_error, Some(false));
    let mut p = CallToolRequestParams::new("wait_inbox");
    p.arguments = json!({"timeout_ms": 5000}).as_object().cloned();
    let r = mcp.call_tool(p).await.unwrap();
    let mail: Value = serde_json::from_str(&r.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(mail[0]["content"], "via mcp");
}

#[tokio::test]
async fn rest_routes_and_openapi_are_served() {
    let (base, _llm) = setup(None).await;
    let doc: Value = reqwest::get(format!("{base}/v1/openapi.json")).await.unwrap().json().await.unwrap();
    assert!(doc["paths"]["/v1/agents/{id}/pause"]["post"].is_object());
    let agents: Value = reqwest::get(format!("{base}/v1/agents")).await.unwrap().json().await.unwrap();
    assert_eq!(agents, json!([]));
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/agents/{}/pause", uuid::Uuid::nil()))
        .json(&json!({"mode":"hard"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn roles_gate_operations() {
    let (base, _llm, hub) = setup_hub(Some("adm")).await;
    let watcher = token(&hub, "user", "watcher").await;
    let v = remote(&base, Some(&watcher), "").await.unwrap();
    assert_eq!(v.call_raw("whoami", Value::Null).await.unwrap()["role"], "viewer");
    let e = v.call_raw("spawn", json!({"type":"helper","prompt":"x"})).await.unwrap_err();
    assert_eq!(e.kind, subnet_ops::ErrorKind::Forbidden);
    // Admin via the bootstrap token.
    let root = remote(&base, Some("adm"), "").await.unwrap();
    assert_eq!(root.call_raw("whoami", Value::Null).await.unwrap(), json!({"addr":"user:root","role":"admin"}));
    // Revoked tokens stop working.
    root.call_raw("revoke_tokens", json!({"kind":"user","name":"watcher"})).await.unwrap();
    assert_eq!(v.call_raw("whoami", Value::Null).await.unwrap_err().kind, subnet_ops::ErrorKind::Unauthorized);
    // Undeclared principals get no tokens.
    let e = root.call_raw("issue_token", json!({"kind":"user","name":"ghost"})).await.unwrap_err();
    assert_eq!(e.kind, subnet_ops::ErrorKind::NotFound);
}

#[tokio::test]
async fn principals_removed_from_the_cluster_lose_access() {
    let (base, _llm, hub) = setup_hub(Some("adm")).await;
    let claude = token(&hub, "client", "claude").await;
    let c = remote(&base, Some(&claude), "").await.unwrap();
    let root = remote(&base, Some("adm"), "").await.unwrap();
    root.call_raw("apply_cluster", json!({"files":[{"name":"p.hcl","text":"user \"watcher\" { role = \"viewer\" }"}]})).await.unwrap();
    assert_eq!(c.call_raw("whoami", Value::Null).await.unwrap_err().kind, subnet_ops::ErrorKind::Unauthorized);
}
