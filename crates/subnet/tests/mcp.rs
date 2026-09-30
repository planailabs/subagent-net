//! Nodes ↔ MCP tool servers (local and routed through the hub), against an
//! in-process rmcp server.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::llm::{last_tool_result, text, tool_call};
use common::net::{Net, agent};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde_json::{Value, json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::PauseMode;
use subnet_core::proto::Op;

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

/// Agent type `base` on nodes s and s2; MCP type `t` (the test server) on
/// `mcp_nodes`; mixture `tooler` binds them. `mcp_extra` and `mix_extra` go
/// into the `mcp` and `mixture` blocks.
fn cluster(mcp_url: &str, mcp_nodes: &[&str], idempotent: &[&str], mcp_extra: &str, mix_extra: &str) -> String {
    format!(
        "node \"s\" {{}}\nnode \"s2\" {{}}\nnode \"m\" {{}}\n{}\nmcp \"t\" {{\n  url = {mcp_url:?}\n  nodes = {mcp_nodes:?}\n  idempotent = {idempotent:?}\n  {mcp_extra}\n}}\nmixture \"tooler\" {{\n  agent = \"base\"\n  mcp = [\"t\"]\n  {mix_extra}\n}}\n",
        agent("base", SYS, "{llm}", &["s", "s2"], ""),
    )
}

struct Env {
    net: Net,
    stats: Arc<Stats>,
}

impl std::ops::Deref for Env {
    type Target = Net;
    fn deref(&self) -> &Net {
        &self.net
    }
}

impl Env {
    /// MCP on the agents' node (`s`).
    async fn new(idempotent: &[&str]) -> Self {
        Self::with(&["s", "s2"], idempotent).await
    }

    /// Eager tools, like before lazy loading.
    async fn with(mcp_nodes: &[&str], idempotent: &[&str]) -> Self {
        Self::custom(mcp_nodes, idempotent, "lazy = false", "").await
    }

    async fn custom(mcp_nodes: &[&str], idempotent: &[&str], mcp_extra: &str, mix_extra: &str) -> Self {
        let stats = Arc::new(Stats::default());
        let url = mcp_server(stats.clone()).await;
        Self { net: Net::new(&cluster(&url, mcp_nodes, idempotent, mcp_extra, mix_extra)).await, stats }
    }

    async fn spawn(&self) -> AgentId {
        self.net.spawn("tooler", "go").await
    }

    async fn until(&self, what: &str, f: impl Fn() -> bool) {
        for _ in 0..400 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }
}

#[tokio::test]
async fn mcp_tools_are_offered_prefixed_and_called() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.echo", json!({"text":"hi"})));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawn().await;
    assert_eq!(e.mail().await["content"], "echo: hi");
    let tools = e.llm.requests()[0]["tools"].clone();
    // On the wire, `<mcp>.<tool>` is `<mcp>__<tool>` (OpenAI-compatible names).
    let echo = tools.as_array().unwrap().iter().find(|t| t["function"]["name"] == "t__echo").unwrap();
    assert_eq!(echo["function"]["parameters"]["properties"]["text"]["type"], "string");
    assert!(tools.as_array().unwrap().iter().any(|t| t["function"]["name"] == "spawn_agent"));
}

#[tokio::test]
async fn bad_mcp_arguments_are_a_tool_error() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.echo", json!(["not", "an", "object"])));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawn().await;
    assert!(e.mail().await["content"].as_str().unwrap().starts_with("error: "));
}

#[tokio::test]
async fn bare_agent_type_has_no_mcp_tools() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.echo", json!({"text":"hi"})));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.net.spawn("base", "go").await;
    let c = e.mail().await["content"].as_str().unwrap().to_string();
    assert!(c.contains("unknown tool"), "{c}");
    assert!(!e.llm.requests()[0]["tools"].to_string().contains("t__echo"));
}

#[tokio::test]
async fn spawning_needs_the_mixtures_mcp_to_run_somewhere() {
    let e = Env::with(&["m"], &[]).await;
    e.node("s").await;
    let err = e.hub.op(&Addr::root(), Op::Spawn { ty: "tooler".into(), prompt: "x".into(), tenant: None }).await.unwrap_err();
    assert!(err.contains("no live node runs mcp"), "{err}");
}

#[tokio::test]
async fn remote_mcp_calls_are_routed_through_the_hub() {
    let e = Env::with(&["m"], &[]).await;
    e.node("s").await;
    e.node("m").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.echo", json!({"text":"far away"})));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    let id = e.spawn().await;
    assert_eq!(e.mail().await["content"], "echo: far away");
    assert_eq!(e.t(id).await["node"], "s", "the agent ran on s, the tool on m");
}

#[tokio::test]
async fn hard_pause_cancels_remote_mcp_call() {
    let e = Env::with(&["m"], &[]).await;
    e.node("s").await;
    e.node("m").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.slow", json!({"ms": 10_000})));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    e.until("server-side cancel", || stats.slow_dropped.load(Ordering::SeqCst) == 1).await;
    e.net.until(id, "paused", |t| t["paused"] == true).await;
}

#[tokio::test]
async fn hard_pause_cancels_mcp_call() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.slow", json!({"ms": 10_000})));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    e.until("server-side cancel", || stats.slow_dropped.load(Ordering::SeqCst) == 1).await;
    e.net.until(id, "paused", |t| t["paused"] == true).await;
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(e.mail().await["content"], "[aborted before completion]");
    assert_eq!(stats.slow_finished.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn quick_pause_lets_mcp_call_finish() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.slow", json!({"ms": 300})));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Quick, tree: false }).await.unwrap();
    e.until("slow finished", || stats.slow_finished.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(e.llm.requests().len(), 1, "no LLM call while paused");
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    assert_eq!(e.mail().await["content"], "slept");
}

async fn crash_during_slow(idempotent: bool) -> (Env, Value) {
    let idem: &[&str] = if idempotent { &["slow"] } else { &[] };
    let e = Env::new(idem).await;
    let a = e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "t.slow", json!({"ms": 400})));
    e.spawn().await;
    let stats = e.stats.clone();
    e.until("slow started", || stats.slow_started.load(Ordering::SeqCst) == 1).await;
    e.hub.disconnect(a).await;
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.node("s2").await;
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

fn echo_server() -> String {
    common::example("mcp_echo")
}

#[tokio::test]
async fn stdio_mcp_server_with_env() {
    // SAFETY: tests in this binary don't otherwise read or write this variable.
    unsafe { std::env::set_var("SUBNET_TEST_GREETING", "hello from env") };
    let cluster = format!(
        "node \"s\" {{}}\n{}\nmcp \"echo\" {{\n  command = [{:?}]\n  env = {{ GREETING = \"$SUBNET_TEST_GREETING\", PLAIN = \"lit\" }}\n  nodes = [\"s\"]\n  lazy = false\n}}\nmixture \"tooler\" {{\n  agent = \"base\"\n  mcp = [\"echo\"]\n}}\n",
        agent("base", SYS, "{llm}", &["s"], ""),
        echo_server()
    );
    let n = Net::new(&cluster).await;
    n.node("s").await;
    n.llm.push(SYS, |_| tool_call("c1", "echo.echo", json!({"text":"hi"})));
    n.llm.push(SYS, |_| tool_call("c2", "echo.env", json!({"name":"GREETING"})));
    n.llm.push(SYS, |_| tool_call("c3", "echo.env", json!({"name":"PLAIN"})));
    n.llm.push(SYS, |body| {
        let msgs = body["messages"].as_array().unwrap();
        let results: Vec<_> = msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap()).collect();
        text(&[&results.join(" | ")])
    });
    n.spawn("tooler", "go").await;
    assert_eq!(n.mail().await["content"], "stdio echo: hi | hello from env | lit");
}

#[tokio::test]
async fn missing_credentials_are_reported_by_the_node() {
    let cluster = format!(
        "node \"s\" {{}}\n{}",
        agent("keyed", SYS, "{llm}", &["s"], "").replace(
            "credential {\n",
            "credential {\n    env = \"SUBNET_TEST_SURELY_MISSING_KEY\"\n"
        )
    );
    let n = Net::new(&cluster).await;
    n.node("s").await;
    let nodes = n.hub.list_nodes().await;
    assert!(nodes[0].agents.is_empty());
    let err = nodes[0].errors.values().next().unwrap();
    assert!(err.contains("SUBNET_TEST_SURELY_MISSING_KEY"), "{err}");
    let e = n.hub.op(&Addr::root(), Op::Spawn { ty: "keyed".into(), prompt: "x".into(), tenant: None }).await.unwrap_err();
    assert!(e.contains("no live node"), "{e}");
}

#[tokio::test]
async fn tool_calls_of_one_message_run_in_parallel() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    let slow = json!({"ms": 600});
    e.llm.push(SYS, move |_| common::llm::tool_calls(&[("a", "t.slow", slow.clone()), ("b", "t.slow", slow.clone())]));
    e.llm.push(SYS, |body| {
        let n = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").count();
        text(&[&format!("{n} results")])
    });
    let started = std::time::Instant::now();
    e.spawn().await;
    assert_eq!(e.mail().await["content"], "2 results");
    assert!(started.elapsed() < Duration::from_millis(1100), "took {:?}", started.elapsed());
    assert_eq!(e.stats.slow_finished.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn hard_pause_cancels_every_parallel_call() {
    let e = Env::new(&[]).await;
    e.node("s").await;
    let slow = json!({"ms": 10_000});
    e.llm.push(SYS, move |_| common::llm::tool_calls(&[("a", "t.slow", slow.clone()), ("b", "t.slow", slow.clone())]));
    let id = e.spawn().await;
    let stats = e.stats.clone();
    e.until("both started", || stats.slow_started.load(Ordering::SeqCst) == 2).await;
    e.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    e.until("both cancelled", || stats.slow_dropped.load(Ordering::SeqCst) == 2).await;
    e.net.until(id, "paused", |t| t["paused"] == true).await;
}

fn offered(body: &Value) -> Vec<String> {
    body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn lazy_tools_are_loaded_and_called_through_call_tool() {
    let e = Env::custom(&["s", "s2"], &[], "", "").await;
    e.node("s").await;
    e.llm.push(SYS, |_| tool_call("c1", "load_tools", json!({"names": ["t.echo"]})));
    e.llm.push(SYS, |_| tool_call("c2", "call_tool", json!({"name": "t.echo", "arguments": {"text": "hi"}})));
    e.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    e.spawn().await;
    assert_eq!(e.mail().await["content"], "echo: hi");
    let r = e.llm.requests();
    let first = offered(&r[0]);
    assert!(first.contains(&"load_tools".to_string()) && first.contains(&"call_tool".to_string()) && !first.contains(&"t__echo".to_string()), "{first:?}");
    let load = r[0]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "load_tools").unwrap();
    assert!(load["function"]["description"].as_str().unwrap().contains("- t.echo: "), "the catalogue lists it");
    // The tool list stays byte-identical (the provider's prompt cache covers it), and the schema is in the conversation.
    assert_eq!(r[1]["tools"], r[0]["tools"]);
    assert_eq!(r[2]["tools"], r[0]["tools"]);
    let loaded = r[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().to_string();
    assert!(loaded.contains(r#""name":"t.echo""#) && loaded.contains("parameters"), "{loaded}");
}

/// Similar texts share words (hashed into a few dimensions).
struct Words;
impl subnet::hub::router::Embed for Words {
    fn embed(&self, texts: &[String], _: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 64];
                for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
                    v[w.bytes().fold(7usize, |h, b| h.wrapping_mul(31) ^ b as usize) % 64] += 1.0;
                }
                v
            })
            .collect())
    }
}

#[tokio::test]
async fn the_router_preloads_matching_tools() {
    let e = Env::custom(&["s", "s2"], &[], "", "router {\n    top_k = 1\n    min_score = 0.2\n  }").await;
    e.hub.set_router(subnet::hub::router::Router::with(Arc::new(Words)));
    e.node("s").await;
    e.llm.push(SYS, |_| text(&["ok"]));
    e.llm.push(SYS, |_| text(&["ok"]));
    // "echo" matches the echo tool's name and description; nothing else does.
    let id = e.net.spawn("tooler", "please echo this text back").await;
    e.mail().await;
    let r = &e.llm.requests()[0];
    // The matching tool's schema comes as a note before the message; the tool list doesn't change.
    let note = r["messages"].as_array().unwrap().iter().find(|m| m["content"].as_str().is_some_and(|c| c.starts_with("[tools loaded"))).expect("a note");
    let note = note["content"].as_str().unwrap();
    assert!(note.contains(r#""name":"t.echo""#), "{note}");
    assert!(!note.contains("t.slow"), "only the top match: {note}");
    assert!(!offered(r).contains(&"t__echo".to_string()));
    let _ = id;
}

#[tokio::test]
async fn per_tenant_servers_run_once_per_tenant() {
    per_tenant("s", "s").await;
}

#[tokio::test]
async fn per_tenant_servers_through_the_hub() {
    per_tenant("a", "s").await;
}

/// Agents on `agent_node`, the per-tenant server on `mcp_node`.
async fn per_tenant(agent_node: &str, mcp_node: &str) {
    let cluster = format!(
        "node \"s\" {{}}\nnode \"a\" {{}}\n{}\nmcp \"who\" {{\n  command = [{:?}]\n  env = {{ WHO = \"t-${{TENANT}}\" }}\n  per_tenant = true\n  default_tenant = \"base\"\n  nodes = [{mcp_node:?}]\n  lazy = false\n}}\nmixture \"tooler\" {{\n  agent = \"base\"\n  mcp = [\"who\"]\n}}\n",
        agent("base", SYS, "{llm}", &[agent_node], ""),
        echo_server()
    );
    let n = Net::new(&cluster).await;
    n.node("s").await;
    if agent_node != mcp_node {
        n.node(agent_node).await;
    }
    let ask = || {
        n.llm.push(SYS, |_| tool_call("c1", "who.env", json!({"name":"WHO"})));
        n.llm.push(SYS, |body| text(&[&last_tool_result(body)]));
    };
    ask();
    let acme = n.hub.spawn_for(&Addr::root(), "tooler", "who?".into(), Some("acme".into())).await.unwrap();
    assert_eq!(n.mail().await["content"], "t-acme");
    ask();
    n.hub.spawn_for(&Addr::root(), "tooler", "who?".into(), Some("globex".into())).await.unwrap();
    assert_eq!(n.mail().await["content"], "t-globex");
    ask();
    n.spawn("tooler", "who?").await;
    assert_eq!(n.mail().await["content"], "t-base", "no tenant: the default instance");
    let agents = n.hub.list_agents().await;
    assert_eq!(agents.iter().find(|a| a.id == acme.id).unwrap().tenant.as_deref(), Some("acme"));
    // Agents can't choose a tenant for their children.
    let e = n.hub.spawn_for(&Addr::Agent(acme.id), "tooler", "x".into(), Some("other".into())).await.unwrap_err();
    assert!(e.to_string().contains("inherit"), "{e}");
}
