//! Senses on nodes: sources, stages, streams; observed via hub notices.

mod common;

use std::time::Duration;

use common::net::Net;
use serde_json::{Value, json};
use subnet::hub::Notice;
use tokio::sync::broadcast::Receiver;

fn kit() -> String {
    common::example("sensekit")
}

/// A cluster with node `s` and the given sense blocks.
async fn net(senses: &str) -> Net {
    Net::new(&format!("node \"s\" {{}}\n{senses}")).await
}

/// Next `count` events of `sense`.
async fn events(rx: &mut Receiver<Notice>, sense: &str, count: usize) -> Vec<Value> {
    let mut out = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while out.len() < count {
        let n = tokio::time::timeout_at(deadline, rx.recv()).await.expect("timed out waiting for sense events").unwrap();
        if let Notice::Sense { sense: s, data, .. } = n
            && s == sense
        {
            out.push(data);
        }
    }
    out
}

#[tokio::test]
async fn exec_source_with_filter_and_map_stages() {
    let n = net(&format!(
        "sense \"door\" {{\n  node = \"s\"\n  source {{ exec = [{:?}, \"emit\", \"{{\\\"state\\\":\\\"open\\\"}}\", \"{{\\\"state\\\":\\\"open\\\"}}\", \"{{\\\"state\\\":\\\"closed\\\"}}\", \"not json\"] }}\n  stage \"bounce\" {{ filter = \"prev == null || event.state != prev.state\" }}\n  stage \"shape\" {{ map = \"{{'door': event.state}}\" }}\n}}\n",
        kit()
    ))
    .await;
    let mut rx = n.hub.subscribe();
    n.node("s").await;
    // The repeated "open" is filtered; "not json" becomes {"line": …}, which
    // the filter can't evaluate (no `state`), so that event is dropped too.
    let got = events(&mut rx, "door", 2).await;
    assert_eq!(got, vec![json!({"door":"open"}), json!({"door":"closed"})]);
}

#[tokio::test]
async fn timer_source_ticks() {
    let n = net("sense \"tick\" {\n  node = \"s\"\n  source { timer = { every = \"100ms\" } }\n}\n").await;
    let mut rx = n.hub.subscribe();
    n.node("s").await;
    let got = events(&mut rx, "tick", 3).await;
    assert_eq!(got.iter().map(|e| e["tick"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
}

#[tokio::test]
async fn webhook_source() {
    let n = net("sense \"gh\" {\n  node = \"s\"\n  source { webhook = { path = \"/github\" } }\n}\n").await;
    let mut rx = n.hub.subscribe();
    let node = std::sync::Arc::new(subnet::node::Node::new("s", None));
    subnet::node::attach(n.hub.clone(), node.clone()).await.unwrap();
    common::net::wait_configured(&n.hub, "s").await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let app = node.senses.webhook_router();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let c = reqwest::Client::new();
    assert_eq!(c.post(format!("{base}/hooks/nope")).body("{}").send().await.unwrap().status(), 404);
    let r = c.post(format!("{base}/hooks/github")).json(&json!({"action":"opened"})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    c.post(format!("{base}/hooks/github")).body("plain text").send().await.unwrap();
    assert_eq!(events(&mut rx, "gh", 2).await, vec![json!({"action":"opened"}), json!({"body":"plain text"})]);
}

#[tokio::test]
async fn file_source_reports_changes_matching_the_glob() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("watch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let n = net(&format!(
        "sense \"inbox\" {{\n  node = \"s\"\n  source {{ file = {{ path = {:?}, glob = \"*.txt\" }} }}\n}}\n",
        dir.to_string_lossy()
    ))
    .await;
    let mut rx = n.hub.subscribe();
    n.node("s").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(dir.join("ignored.bin"), b"x").unwrap();
    std::fs::write(dir.join("note.txt"), b"hello").unwrap();
    let got = events(&mut rx, "inbox", 1).await;
    assert!(got[0]["path"].as_str().unwrap().ends_with("note.txt"), "{got:?}");
    assert_eq!(got[0]["kind"], "create");
}

#[tokio::test]
async fn stream_to_stage_on_the_same_node() {
    let n = net(&format!(
        "sense \"mic\" {{\n  node = \"s\"\n  source {{\n    exec = [{k:?}, \"stream\", \"hi\", \"50\"]\n    stream = \"text\"\n  }}\n}}\nsense \"heard\" {{\n  node = \"s\"\n  source {{ stream = \"mic\" }}\n  stage \"stt\" {{ exec = [{k:?}, \"words\"] }}\n  stage \"only\" {{ filter = \"size(event.text) > 0\" }}\n}}\n",
        k = kit()
    ))
    .await;
    let mut rx = n.hub.subscribe();
    n.node("s").await;
    let got = events(&mut rx, "heard", 2).await;
    assert!(got.iter().all(|e| e["text"].as_str().unwrap().contains("hi")), "{got:?}");
    // The stream itself never reaches the hub as events.
    while let Ok(n) = rx.try_recv() {
        assert!(!matches!(n, Notice::Sense { ref sense, .. } if sense == "mic"));
    }
}

#[tokio::test]
async fn exec_stage_transforms_events() {
    let n = net(&format!(
        "sense \"nums\" {{\n  node = \"s\"\n  source {{ exec = [{k:?}, \"emit\", \"{{\\\"n\\\":1}}\", \"{{\\\"n\\\":21}}\"] }}\n  stage \"x2\" {{ exec = [{k:?}, \"double\"] }}\n}}\n",
        k = kit()
    ))
    .await;
    let mut rx = n.hub.subscribe();
    n.node("s").await;
    assert_eq!(events(&mut rx, "nums", 2).await, vec![json!({"n":2}), json!({"n":42})]);
}

#[tokio::test]
async fn broken_senses_are_reported() {
    let n = net("sense \"bad\" {\n  node = \"s\"\n  source { exec = [\"/surely/not/a/program\"] }\n}\n").await;
    n.node("s").await;
    for _ in 0..100 {
        let nodes = n.hub.list_nodes().await;
        if let Some(Some(e)) = nodes[0].senses.get("bad") {
            assert!(e.contains("starting"), "{e}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("sense error never reported");
}
