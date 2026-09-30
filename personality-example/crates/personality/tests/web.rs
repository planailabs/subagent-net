//! Firecrawl through its MCP server: Vesper looks something up. The real
//! `firecrawl-mcp` package (via npx) talks to a stand-in Firecrawl API
//! (`FIRECRAWL_API_URL`, as for a self-hosted instance).

mod common;

use std::sync::{Arc, Mutex};

use common::{brain, launch, tool_result, wait, with_env};
use futures::StreamExt;
use personality::CLUSTER;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

fn script(message: &str) -> Vec<(&'static str, Value)> {
    if message.contains("woke_up") {
        vec![("world.look_around", json!({}))]
    } else if message.contains("who wrote carmilla") {
        vec![
            ("web.firecrawl_search", json!({"query": "who wrote Carmilla", "limit": 3})),
            ("world.say", json!({"text": "Sheridan Le Fanu, in 1872. Wikipedia says so, anyway.", "to": "alice"})),
        ]
    } else {
        vec![]
    }
}

type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;

/// A stand-in for api.firecrawl.dev on `port`: records requests, answers
/// searches.
async fn firecrawl(port: u16) -> Seen {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let s = s.clone();
        async move {
            let path = req.uri().path().to_string();
            let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or_default();
            s.lock().unwrap().push((path, auth, body));
            axum::Json(json!({
                "success": true,
                "data": {"web": [{"url": "https://en.wikipedia.org/wiki/Carmilla", "title": "Carmilla - Wikipedia", "description": "Carmilla is an 1872 Gothic novella by Irish author Sheridan Le Fanu."}]},
            }))
        }
    });
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    seen
}

#[test]
#[ignore = "runs firecrawl-mcp through npx (downloads it)"]
fn she_looks_it_up() {
    // The fake Firecrawl's URL must be in the env before the runtime starts,
    // so it listens on a port picked here.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let api = format!("http://127.0.0.1:{port}");
    with_env(
        "web",
        &[
            ("ROOM_MCP_TOKEN", "web-token"),
            ("SUBJECT_MEMORY_EMBEDDINGS", "off"),
            ("DEEPSEEK_API_KEY", "scripted"),
            ("FIRECRAWL_API_KEY", "fc-test-key"),
            ("FIRECRAWL_API_URL", &api),
        ],
        async move |data| scenario(data, port).await,
    );
}

async fn scenario(data: &std::path::Path, port: u16) {
    let seen = firecrawl(port).await;
    let (llm, log) = brain(script).await;
    let r = launch(data, llm, CLUSTER).await;
    r.room.users.set("alice", "wonderland").unwrap();

    // Alice asks in the chat; Vesper searches the web and answers aloud.
    let http = reqwest::Client::new();
    let res = http.post(format!("{}/api/login", r.room_url)).json(&json!({"name": "alice", "password": "wonderland"})).send().await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let mut req = format!("{}/ws", r.room_url.replace("http", "ws")).into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // She wakes once the node has started its MCP servers, the web one
    // included (the first npx run downloads it).
    wait("the wake-up look_around", || tool_result(&log, "world.look_around").is_some()).await;
    futures::SinkExt::send(&mut ws, Message::text(json!({"type": "chat", "text": "who wrote carmilla?"}).to_string())).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let m = tokio::time::timeout_at(deadline, ws.next()).await.expect("no answer").unwrap().unwrap();
        let Message::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "speech" {
            assert!(v["text"].as_str().unwrap().contains("Le Fanu"));
            break;
        }
    }
    let result = tool_result(&log, "web.firecrawl_search").expect("the search ran");
    assert!(result.contains("Sheridan Le Fanu"), "the search result reached her: {result}");
    let calls = seen.lock().unwrap().clone();
    let (path, auth, body) = calls.iter().find(|(p, _, _)| p.ends_with("/search")).unwrap_or_else(|| panic!("no search reached Firecrawl: {calls:?}"));
    assert!(path.starts_with("/v"), "{path}");
    assert_eq!(auth, "Bearer fc-test-key");
    assert_eq!(body["query"], "who wrote Carmilla");
    r.hub.shutdown();
}
