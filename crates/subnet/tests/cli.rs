//! The `subnet` binary's client commands against a live hub.
// Spawning the binary is the point of this test.

mod common;

use std::sync::Arc;

use common::db_url;
use common::llm::MockLlm;
use subnet::hub::{Hub, http};
use subnet::spawner::config::{Config, TypeConfig};
use subnet::spawner::{Spawner, attach};
use subnet_core::agent::Budget;
use subnet_llm::ModelConfig;

async fn setup() -> (String, MockLlm) {
    let llm = MockLlm::start().await;
    let hub = Hub::open(&db_url().await, Some("t0k".into())).await.unwrap();
    let t = TypeConfig {
        name: "helper".into(),
        description: String::new(),
        system: "sys".into(),
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
    let (ok, out, err) = subnet(&base, &["types"]).await;
    assert!(ok, "{err}");
    assert!(out.contains("\"helper\""), "{out}");
    let (ok, out, err) = subnet(&base, &["spawn", "helper", "hello", "--wait"]).await;
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "cli works");
    let (ok, out, _) = subnet(&base, &["agents"]).await;
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
    assert!(err.contains("no live spawner"), "{err}");
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_subnet"))
        .args(["types"])
        .env("SUBNET_HUB", &base)
        .env("SUBNET_TOKEN", "wrong")
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
}
