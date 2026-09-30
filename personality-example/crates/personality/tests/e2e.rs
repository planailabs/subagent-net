//! End to end: a private Postgres, the hub and node, the room server, the
//! cluster file, and a scripted brain in place of DeepSeek. Alice logs in,
//! says "make me a coffee" into her microphone, and Vesper walks to the
//! kitchen, brews, waits for the coffee maker, pours, brings it over, says
//! so (Alice's browser gets the speech) and remembers who asked.

mod common;

use std::time::Duration;

use common::{brain, launch, tool_result, wait, with_env};
use futures::{SinkExt, StreamExt};
use personality::CLUSTER;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Vesper's scripted mind: which tool calls follow which message.
fn script(message: &str) -> Vec<(&'static str, Value)> {
    if message.contains("woke_up") {
        vec![("world.look_around", json!({}))]
    } else if message.contains("make me a coffee") {
        vec![
            ("memory.recall", json!({"query": "alice coffee", "about": "person:alice"})),
            ("world.move_to", json!({"target": "coffee_maker"})),
            ("world.interact", json!({"object": "mug", "action": "pick_up"})),
            ("world.interact", json!({"object": "coffee_maker", "action": "brew"})),
            ("world.say", json!({"text": "Coffee's on. Twenty seconds.", "to": "alice"})),
        ]
    } else if message.contains("coffee_ready") {
        vec![
            ("world.interact", json!({"object": "coffee_maker", "action": "pour"})),
            ("world.move_to", json!({"target": "alice"})),
            ("world.say", json!({"text": "Here you go, black as my dress.", "to": "alice"})),
            ("memory.remember", json!({"title": "Alice", "body": "Asked me for a coffee by voice. Takes it black.", "about": "person:alice"})),
        ]
    } else {
        vec![]
    }
}

#[test]
fn make_me_a_coffee() {
    // No FIRECRAWL_API_KEY: the web MCP server is unavailable, and she
    // manages without it.
    with_env("e2e", &[("ROOM_MCP_TOKEN", "e2e-token"), ("SUBJECT_MEMORY_EMBEDDINGS", "off"), ("DEEPSEEK_API_KEY", "scripted")], scenario);
}

async fn scenario(data: &std::path::Path) {
    let (llm, log) = brain(script).await;
    let cluster = CLUSTER.replace("\"stt\", \"--models\"", "\"stt\", \"--fake\", \"could you make me a coffee\", \"--models\"");
    let r = launch(data, llm, &cluster).await;
    r.room.users.set("alice", "wonderland").unwrap();

    // She wakes up and looks around.
    wait("the wake-up look_around", || tool_result(&log, "world.look_around").is_some_and(|t| t.contains("coffee_maker"))).await;

    // Alice logs in and talks into her microphone.
    let http = reqwest::Client::new();
    let res = http.post(format!("{}/api/login", r.room_url)).json(&json!({"name": "alice", "password": "wonderland"})).send().await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let mut req = format!("{}/ws", r.room_url.replace("http", "ws")).into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text(json!({"type": "voice_start"}).to_string())).await.unwrap();
    ws.send(Message::binary(vec![0u8; 32000])).await.unwrap();
    ws.send(Message::text(json!({"type": "voice_end"}).to_string())).await.unwrap();

    // She hears it (through stt), walks, brews, waits, pours, brings it and says so.
    let mut heard = vec![];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let final_state = loop {
        let m = tokio::time::timeout_at(deadline, ws.next()).await.expect("no coffee within 90 s").unwrap().unwrap();
        let Message::Text(t) = m else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "speech" {
            heard.push(v["text"].as_str().unwrap().to_string());
        }
        if v["type"] == "state" && heard.iter().any(|h| h.starts_with("Here you go")) {
            break v;
        }
    };
    assert_eq!(heard, ["Coffee's on. Twenty seconds.", "Here you go, black as my dress."]);
    let mug = final_state["objects"].as_array().unwrap().iter().find(|o| o["id"] == "mug").unwrap();
    assert_eq!(mug["state"], json!({"kind": "mug", "held": true, "coffee": true}));
    assert!(final_state["vesper"]["pos"][1].as_f64().unwrap() > 1.5, "she came to the front: {}", final_state["vesper"]);

    // The whole chain was real: the voice went through stt, the coffee
    // event through the world webhook, and memory is written.
    let asked = log.lock().unwrap().iter().any(|b| b.to_string().contains(r#"\"via\":\"voice\""#));
    assert!(asked, "the voice message reached her");
    assert!(tool_result(&log, "memory.recall").is_some());
    assert!(tool_result(&log, "world.interact").is_some());
    wait("the memory about alice", || tool_result(&log, "memory.remember").is_some_and(|t| t.contains("person:alice"))).await;
    r.hub.shutdown();
}
