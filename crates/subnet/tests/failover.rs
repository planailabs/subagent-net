//! Real processes: a hub and spawners talking over WebSockets. Processes are
//! killed hard mid-stream and the agent must finish elsewhere with its partial
//! output intact. (Spawning the binary is the point of this test.)

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::db_url;
use common::llm::MockLlm;
use serde_json::{Value, json};
use subnet::client::Client;
use tokio::process::{Child, Command};

const SYS: &str = "failover worker";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_subnet"));
    c.env("RUST_LOG", "warn").stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    c
}

fn hub(db: &str, port: u16) -> Child {
    bin()
        .args(["hub", "--db", db, "--listen", &format!("127.0.0.1:{port}")])
        .env("SUBNET_ADMIN_TOKEN", "tok")
        .spawn()
        .unwrap()
}

fn node(name: &str, port: u16) -> Child {
    bin()
        .args(["node", "--name", name])
        .env("SUBNET_HUB", format!("http://127.0.0.1:{port}"))
        .env("SUBNET_TOKEN", "tok")
        .spawn()
        .unwrap()
}

/// Worker type on nodes a, b and s (capacity 1 each).
/// Until some node reports it can run the worker type.
async fn wait_node(r: &Client) {
    for _ in 0..200 {
        let nodes = r.call_raw("list_nodes", Value::Null).await.unwrap();
        if nodes.as_array().unwrap().iter().any(|n| n["agents"].as_array().is_some_and(|a| !a.is_empty())) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no node became ready");
}

async fn apply(r: &Client, llm: &str) {
    let mut text = String::new();
    for n in ["a", "b", "s"] {
        text.push_str(&format!("node {n:?} {{ capacity = 1 }}\n"));
    }
    text.push_str(&common::net::agent("worker", SYS, llm, &["a", "b", "s"], ""));
    r.call_raw("apply_cluster", json!({"files": [{"name": "c.hcl", "text": text}]})).await.unwrap();
}

async fn connect(port: u16) -> Client {
    for _ in 0..100 {
        if let Ok(r) = remote(&format!("http://127.0.0.1:{port}"), Some("tok"), "user").await {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("hub did not come up");
}

async fn until(r: &Client, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..300 {
        if let Ok(v) = r.call_raw("list_agents", Value::Null).await
            && f(&v)
        {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn partial_len(r: &Client, id: &str) -> usize {
    let t = r.call_raw("transcript", json!({"id": id})).await.unwrap();
    t["partial"]["content"].as_str().map_or(0, str::len)
}

#[tokio::test]
async fn killed_node_hands_agent_over_with_partial() {
    let llm = MockLlm::start().await;
    llm.set_gap(Duration::from_millis(200));
    llm.say(SYS, &["one ", "two ", "three ", "four ", "five ", "six ", "seven ", "eight"]);
    let port = free_port();
    let _hub = hub(&db_url().await, port);
    let r = connect(port).await;
    apply(&r, &llm.url).await;
    let mut a = node("a", port);
    wait_node(&r).await;
    let id =
        r.call_raw("spawn", json!({"type":"worker","prompt":"count"})).await.unwrap()["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if partial_len(&r, &id).await >= 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = partial_len(&r, &id).await;
    assert!(before >= 8, "no partial output streamed yet");

    llm.set_gap(Duration::ZERO);
    llm.say(SYS, &["<continued>"]);
    a.kill().await.unwrap(); // SIGKILL / TerminateProcess
    let _b = node("b", port);
    let mail = r.call_raw("wait_inbox", json!({"timeout_ms": 20_000})).await.unwrap();
    let c = mail[0]["content"].as_str().unwrap();
    assert!(c.starts_with("one two"), "{c}");
    assert!(c.ends_with("<continued>"), "{c}");
    assert!(c.len() >= before + "<continued>".len());
    let agents = until(&r, "agent on b", |v| v[0]["node"] == "b").await;
    assert_eq!(agents[0]["phase"], "idle");
}

#[tokio::test]
async fn killed_hub_restarts_and_node_reconnects() {
    let llm = MockLlm::start().await;
    llm.set_gap(Duration::from_millis(200));
    llm.say(SYS, &["alpha ", "beta ", "gamma ", "delta ", "epsilon ", "zeta"]);
    let db = db_url().await;
    let port = free_port();
    let mut h = hub(&db, port);
    let r = connect(port).await;
    apply(&r, &llm.url).await;
    let _s = node("s", port);
    wait_node(&r).await;
    let id =
        r.call_raw("spawn", json!({"type":"worker","prompt":"greek"})).await.unwrap()["id"].as_str().unwrap().to_string();
    for _ in 0..100 {
        if partial_len(&r, &id).await >= 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(partial_len(&r, &id).await >= 6);

    llm.set_gap(Duration::ZERO);
    llm.say(SYS, &["<after restart>"]);
    h.kill().await.unwrap();
    drop(r);
    let _h2 = hub(&db, port);
    let r = connect(port).await;
    // The spawner reconnects on its own (backoff ≤ 2s) and gets the agent back.
    let mail = r.call_raw("wait_inbox", json!({"timeout_ms": 20_000})).await.unwrap();
    let c = mail[0]["content"].as_str().unwrap();
    assert!(c.starts_with("alpha "), "{c}");
    assert!(c.ends_with("<after restart>"), "{c}");
}

async fn remote(base: &str, token: Option<&str>, who: &str) -> Result<Client, subnet_ops::OpError> {
    let _ = who;
    let c = Client::new(base, token.map(String::from));
    // Fail early like a session handshake would.
    c.call_raw("list_types", serde_json::Value::Null).await?;
    Ok(c)
}
