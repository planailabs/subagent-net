//! The `subnet` binary's client commands against a live hub.
// Spawning the binary is the point of this test.

mod common;

use common::db_url;
use common::llm::MockLlm;
use subnet::hub::db::ClusterFile;
use subnet::hub::{Hub, http};
use subnet_core::addr::Addr;

async fn setup() -> (String, MockLlm) {
    let llm = MockLlm::start().await;
    let hub = Hub::open(&db_url().await, Some("t0k".into())).await.unwrap();
    let text = format!("node \"s\" {{}}\n{}", common::net::agent("helper", "sys", &llm.url, &["s"], ""));
    hub.apply_cluster(vec![ClusterFile { name: "c.hcl".into(), text }], false, &Addr::root()).await.unwrap();
    common::net::node_ready(&hub, "s").await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, http::router(hub)).await.unwrap() });
    (base, llm)
}

async fn subnet(base: &str, args: &[&str]) -> (bool, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_subnet"))
        .args(args)
        .env("SUBNET_HUB", base)
        .env("SUBNET_TOKEN", "t0k")
        .env("RUST_LOG", "warn")
        .output()
        .await
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

#[tokio::test]
async fn cli_round_trip() {
    let (base, llm) = setup().await;
    llm.say("sys", &["cli ", "works"]);
    let (ok, out, err) = subnet(&base, &["list-types"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("\"helper\""), "{out}");
    let (ok, out, err) = subnet(&base, &["spawn", "helper", "hello", "--wait"]).await;
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "cli works");
    let (ok, out, _) = subnet(&base, &["list-agents"]).await;
    assert!(ok);
    let agents: serde_json::Value = serde_json::from_str(&out).unwrap();
    let id = agents[0]["id"].as_str().unwrap().to_string();
    let (ok, _, err) = subnet(&base, &["pause", &id, "--mode", "hard"]).await;
    assert!(ok, "{err}");
    let (_, out, _) = subnet(&base, &["transcript", &id]).await;
    assert!(out.contains("\"pause\": \"hard\""), "{out}");
    let (ok, _, _) = subnet(&base, &["resume", &id]).await;
    assert!(ok);
}

#[tokio::test]
async fn cli_reports_errors() {
    let (base, _llm) = setup().await;
    let (ok, _, err) = subnet(&base, &["spawn", "ghost", "boo"]).await;
    assert!(!ok);
    assert!(err.contains("no mixture or agent type"), "{err}");
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_subnet"))
        .args(["list-types"])
        .env("SUBNET_HUB", &base)
        .env("SUBNET_TOKEN", "wrong")
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
}

#[tokio::test]
async fn watch_prints_the_transcript_live() {
    use tokio::io::AsyncReadExt;
    let (base, llm) = setup().await;
    llm.set_gap(std::time::Duration::from_millis(100));
    llm.say("sys", &["watch ", "me ", "stream"]);
    let (ok, out, err) = subnet(&base, &["spawn", "helper", "go"]).await;
    assert!(ok, "{err}");
    let id = serde_json::from_str::<serde_json::Value>(&out).unwrap()["id"].as_str().unwrap().to_string();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_subnet"))
        .args(["watch", &id])
        .env("SUBNET_HUB", &base)
        .env("SUBNET_TOKEN", "t0k")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !seen.contains("· idle") || !seen.contains("watch me stream") {
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout_at(deadline, stdout.read(&mut buf)).await.unwrap_or_else(|_| panic!("got only: {seen:?}")).unwrap();
        assert!(n > 0, "watch exited: {seen:?}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("▸ ") && seen.contains("go"), "the user message: {seen:?}");
    assert_eq!(seen.matches("watch me stream").count(), 1, "streamed text isn't repeated: {seen:?}");
    assert!(!seen.contains("\x1b["), "no colour when piped");
}
