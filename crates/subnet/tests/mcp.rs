//! Spawner ↔ MCP tool servers, against an in-process rmcp server.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::llm::{MockLlm, last_tool_result, text, tool_call};
use common::{db_url, id_of};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde_json::{Value, json};
use subnet::hub::Hub;
use subnet::spawner::config::{Config, McpServerConfig, TypeConfig};
use subnet::spawner::{Spawner, attach};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Budget, PauseMode};
use subnet_core::proto::Op;
use subnet_llm::ModelConfig;

#[derive(Default)]
struct Stats {
    slow_started: AtomicUsize,
    slow_finished: AtomicUsize,
    slow_dropped: AtomicUsize,
}

#[derive(Clone)]
struct Tools(Arc<Stats>);

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SlowArgs {
    ms: u64,
}

/// Counts calls that ended before finishing (i.e. cancelled).
struct DropGuard(Arc<Stats>, bool);
impl Drop for DropGuard {
    fn drop(&mut self) {
        if !self.1 {
            self.0.slow_dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[tool_router(server_handler)]
impl Tools {
    #[tool(description = "Echo text back")]
    fn echo(&self, Parameters(EchoArgs { text }): Parameters<EchoArgs>) -> String {
        format!("echo: {text}")
    }

    #[tool(description = "Sleep for ms milliseconds")]
    async fn slow(&self, Parameters(SlowArgs { ms }): Parameters<SlowArgs>, ctx: RequestContext<RoleServer>) -> String {
        self.0.slow_started.fetch_add(1, Ordering::SeqCst);
        let mut g = DropGuard(self.0.clone(), false);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
            // Fires when the client sends notifications/cancelled.
            _ = ctx.ct.cancelled() => return "cancelled".into(),
        }
        g.1 = true;
        self.0.slow_finished.fetch_add(1, Ordering::SeqCst);
        "slept".into()
    }
}

async fn mcp_server(stats: Arc<Stats>) -> String {
    let service = StreamableHttpService::new(
        move || Ok(Tools(stats.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, axum::Router::new().nest_service("/mcp", service)).await.unwrap() });
    url
}

const SYS: &str = "tool user";

fn cfg(llm: &str, mcp: &str, idempotent: &[&str]) -> Config {
    Config {
        hub: String::new(),
        name: "s".into(),
        capacity: 4,
        token_env: None,
        types: vec![TypeConfig {
            name: "tooler".into(),
            description: String::new(),
            system: SYS.into(),
            model: ModelConfig { base_url: llm.into(), model: "m".into(), api_key_env: None, prefill: false, params: Default::default() },
            mcp: vec![McpServerConfig { name: "t".into(), command: None, args: vec![], env: Default::default(), url: Some(mcp.into()) }],
            spawns: vec![],
            budget: Budget::default(),
            approve: vec![],
            idempotent: idempotent.iter().map(|s| s.to_string()).collect(),
        }],
    }
}

struct Env {
    hub: Arc<Hub>,
    llm: MockLlm,
    stats: Arc<Stats>,
    mcp: String,
}

impl Env {
    async fn new() -> Self {
        let stats = Arc::new(Stats::default());
        let mcp = mcp_server(stats.clone()).await;
        Self { hub: Hub::open(&db_url().await, None).await.unwrap(), llm: MockLlm::start().await, stats, mcp }
    }
    async fn spawner(&self, idempotent: &[&str]) -> u64 {
        let sp = Spawner::new(&cfg(&self.llm.url, &self.mcp, idempotent)).await.unwrap();
        attach(self.hub.clone(), Arc::new(sp)).await.unwrap()
    }
    async fn spawn(&self) -> AgentId {
        id_of(&self.hub.op(&Addr::User, Op::Spawn { ty: "tooler".into(), prompt: "go".into() }).await.unwrap())
    }
    async fn mail(&self) -> Value {
        let m = self.hub.op(&Addr::User, Op::WaitInbox { timeout_ms: Some(10_000) }).await.unwrap();
        assert!(!m.as_array().unwrap().is_empty(), "no mail");
        m[0].clone()
    }
    async fn until(&self, what: &str, f: impl Fn() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }
}

#[tokio::test]
async fn mcp_tool_is_offered_and_called() {
    let e = Env::new().await;
    e.spawner(&[]).await;
    e.llm.push(SYS, |_| tool_call("c1", "echo", json!({"text":"hi"})));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawn().await;
    assert_eq!(e.mail().await["content"], "echo: hi");
    let tools = e.llm.requests()[0]["tools"].clone();
    let echo = tools.as_array().unwrap().iter().find(|t| t["function"]["name"] == "echo").unwrap();
    assert_eq!(echo["function"]["parameters"]["properties"]["text"]["type"], "string");
}

#[tokio::test]
async fn bad_mcp_arguments_are_a_tool_error() {
    let e = Env::new().await;
    e.spawner(&[]).await;
    e.llm.push(SYS, |_| tool_call("c1", "echo", json!(["not", "an", "object"])));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawn().await;
    assert!(e.mail().await["content"].as_str().unwrap().starts_with("error: "));
}

#[tokio::test]
async fn hard_pause_cancels_mcp_call() {
    let e = Env::new().await;
    e.spawner(&[]).await;
    e.llm.push(SYS, |_| tool_call("c1", "slow", json!({"ms": 10_000})));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.op(&Addr::User, Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    e.until("server-side cancel", || stats.slow_dropped.load(Ordering::SeqCst) == 1).await;
    for _ in 0..100 {
        if e.hub.op(&Addr::User, Op::Transcript { id }).await.unwrap()["paused"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.hub.op(&Addr::User, Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(e.mail().await["content"], "[aborted before completion]");
    assert_eq!(stats.slow_finished.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn quick_pause_lets_mcp_call_finish() {
    let e = Env::new().await;
    e.spawner(&[]).await;
    e.llm.push(SYS, |_| tool_call("c1", "slow", json!({"ms": 300})));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.op(&Addr::User, Op::Pause { id, mode: PauseMode::Quick, tree: false }).await.unwrap();
    e.until("slow finished", || stats.slow_finished.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(e.llm.requests().len(), 1, "no LLM call while paused");
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.hub.op(&Addr::User, Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(e.mail().await["content"], "slept");
}

async fn crash_during_slow(idempotent: bool) -> (Env, Value) {
    let e = Env::new().await;
    let idem: &[&str] = if idempotent { &["slow"] } else { &[] };
    let a = e.spawner(idem).await;
    e.llm.push(SYS, |_| tool_call("c1", "slow", json!({"ms": 400})));
    e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.disconnect(a).await;
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawner(idem).await;
    let m = e.mail().await;
    (e, m)
}

#[tokio::test]
async fn crash_mid_call_reruns_idempotent_tool() {
    let (e, m) = crash_during_slow(true).await;
    assert_eq!(m["content"], "slept");
    assert_eq!(e.stats.slow_started.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn crash_mid_call_does_not_rerun_other_tools() {
    let (e, m) = crash_during_slow(false).await;
    assert_eq!(m["content"], "[aborted before completion]");
    assert_eq!(e.stats.slow_started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tool_shadowing_a_builtin_is_rejected() {
    #[derive(Clone)]
    struct Bad;
    #[tool_router(server_handler)]
    impl Bad {
        #[tool(description = "clash")]
        fn list_agents(&self) -> String {
            String::new()
        }
    }
    let service = StreamableHttpService::new(|| Ok(Bad), LocalSessionManager::default().into(), StreamableHttpServerConfig::default());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, axum::Router::new().nest_service("/mcp", service)).await.unwrap() });
    let err = Spawner::new(&cfg("http://unused", &url, &[])).await.err().unwrap();
    assert!(err.to_string().contains("shadows a built-in"), "{err}");
}

/// Path of the `mcp_echo` example, which `cargo test` builds alongside.
fn echo_server() -> String {
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_subnet"));
    let p = bin.parent().unwrap().join("examples").join(format!("mcp_echo{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "build the example first: cargo build --example mcp_echo ({})", p.display());
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn stdio_mcp_server_with_env() {
    let hub = Hub::open(&db_url().await, None).await.unwrap();
    let llm = MockLlm::start().await;
    let mut c = cfg(&llm.url, "unused", &[]);
    c.types[0].mcp = vec![McpServerConfig {
        name: "echo".into(),
        command: Some(echo_server()),
        args: vec![],
        env: [("GREETING".to_string(), "hello from env".to_string())].into(),
        url: None,
    }];
    attach(hub.clone(), Arc::new(Spawner::new(&c).await.unwrap())).await.unwrap();
    llm.push(SYS, |_| tool_call("c1", "echo", json!({"text":"hi"})));
    llm.push(SYS, |_| tool_call("c2", "env", json!({"name":"GREETING"})));
    llm.push(SYS, |body| {
        let msgs = body["messages"].as_array().unwrap();
        let results: Vec<_> = msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap()).collect();
        text(&[&results.join(" | ")])
    });
    hub.op(&Addr::User, Op::Spawn { ty: "tooler".into(), prompt: "go".into() }).await.unwrap();
    let m = hub.op(&Addr::User, Op::WaitInbox { timeout_ms: Some(10_000) }).await.unwrap();
    assert_eq!(m[0]["content"], "stdio echo: hi | hello from env");
}
