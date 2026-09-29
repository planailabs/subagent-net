//! Real processes: a hub and spawners talking over WebSockets. Processes are
//! killed hard mid-stream and the agent must finish elsewhere with its partial
//! output intact. (Spawning the binary is the point of this test.)

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use common::db_url;
use common::llm::MockLlm;
use serde_json::{Value, json};
use subnet::client::Remote;
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
    bin().args(["hub", "--db", db, "--listen", &format!("127.0.0.1:{port}")]).env("SUBNET_TOKEN", "tok").spawn().unwrap()
}

fn spawner(name: &str, port: u16, llm: &str) -> Child {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("failover-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("spawner.toml");
    std::fs::write(
        &path,
        format!(
            r#"hub = "ws://127.0.0.1:{port}/spawner"
name = "{name}"
capacity = 1
token_env = "SUBNET_TOKEN"

[[type]]
name = "worker"
system = "{SYS}"
model = {{ base_url = "{llm}", model = "m" }}
"#
        ),
    )
    .unwrap();
    bin().arg("spawner").arg("-c").arg(&path).env("SUBNET_TOKEN", "tok").spawn().unwrap()
}

async fn connect(port: u16) -> Remote {
    for _ in 0..100 {
        if let Ok(r) = Remote::connect(&format!("http://127.0.0.1:{port}"), Some("tok"), "user").await {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("hub did not come up");
}

async fn until(r: &Remote, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..300 {
        if let Ok(v) = r.call("list_agents", Value::Null).await
            && f(&v)
        {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn partial_len(r: &Remote, id: &str) -> usize {
    let t = r.call("transcript", json!({"id": id})).await.unwrap();
    t["partial"]["content"].as_str().map_or(0, str::len)
}

#[tokio::test]
async fn killed_spawner_hands_agent_over_with_partial() {
    let llm = MockLlm::start().await;
    llm.set_gap(Duration::from_millis(200));
    llm.say(SYS, &["one ", "two ", "three ", "four ", "five ", "six ", "seven ", "eight"]);
    let port = free_port();
    let _hub = hub(&db_url().await, port);
    let r = connect(port).await;
    let mut a = spawner("a", port, &llm.url);
    for _ in 0..100 {
        if r.call("list_types", Value::Null).await.unwrap().as_array().is_some_and(|t| !t.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let id = r.call("spawn", json!({"type":"worker","prompt":"count"})).await.unwrap()["id"].as_str().unwrap().to_string();
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
    let _b = spawner("b", port, &llm.url);
    let mail = r.call("wait_inbox", json!({"timeout_ms": 20_000})).await.unwrap();
    let c = mail[0]["content"].as_str().unwrap();
    assert!(c.starts_with("one two"), "{c}");
    assert!(c.ends_with("<continued>"), "{c}");
    assert!(c.len() >= before + "<continued>".len());
    let agents = until(&r, "agent on b", |v| v[0]["spawner"] == "b").await;
    assert_eq!(agents[0]["phase"], "idle");
}

#[tokio::test]
async fn killed_hub_restarts_and_spawner_reconnects() {
    let llm = MockLlm::start().await;
    llm.set_gap(Duration::from_millis(200));
    llm.say(SYS, &["alpha ", "beta ", "gamma ", "delta ", "epsilon ", "zeta"]);
    let db = db_url().await;
    let port = free_port();
    let mut h = hub(&db, port);
    let r = connect(port).await;
    let _s = spawner("s", port, &llm.url);
    for _ in 0..100 {
        if r.call("list_types", Value::Null).await.unwrap().as_array().is_some_and(|t| !t.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let id = r.call("spawn", json!({"type":"worker","prompt":"greek"})).await.unwrap()["id"].as_str().unwrap().to_string();
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
    let mail = r.call("wait_inbox", json!({"timeout_ms": 20_000})).await.unwrap();
    let c = mail[0]["content"].as_str().unwrap();
    assert!(c.starts_with("alpha "), "{c}");
    assert!(c.ends_with("<after restart>"), "{c}");
}
