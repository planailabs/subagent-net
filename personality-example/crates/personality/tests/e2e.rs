//! End to end: a private Postgres, the hub and node, the room server, the
//! cluster file, and a scripted brain in place of DeepSeek. Alice logs in,
//! says "make me a coffee" into her microphone, and Vesper walks to the
//! kitchen, brews, waits for the coffee maker, pours, brings it over, says
//! so (Alice's browser gets the speech) and remembers who asked.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use personality::{CLUSTER, Opts, stop_postgres, up};
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

type Log = Arc<Mutex<Vec<Value>>>;

/// An OpenAI-compatible endpoint answering with the script.
async fn brain() -> (String, Log) {
    let log: Log = Arc::default();
    let l2 = log.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = l2.clone();
            async move {
                log.lock().unwrap().push(body.clone());
                let msgs = body["messages"].as_array().unwrap();
                let last_user = msgs.iter().rposition(|m| m["role"] == "user").unwrap();
                let done = msgs[last_user..].iter().filter(|m| m["role"] == "tool").count();
                let steps = script(msgs[last_user]["content"].as_str().unwrap_or_default());
                let chunks: Vec<String> = match steps.get(done) {
                    Some((name, args)) => vec![
                        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("c{}", msgs.len()),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}).to_string(),
                        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
                    ],
                    None => vec![
                        json!({"choices":[{"delta":{"content":"done"}}]}).to_string(),
                        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":1}}).to_string(),
                    ],
                };
                let sse: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect::<String>() + "data: [DONE]\n\n";
                ([("content-type", "text/event-stream")], sse)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, log)
}

/// The tool result that answered a call, from the brain's requests.
fn tool_result(log: &Log, name: &str) -> Option<String> {
    for body in log.lock().unwrap().iter().rev() {
        let msgs = body["messages"].as_array().unwrap();
        for (i, m) in msgs.iter().enumerate() {
            if let Some(call) = m["tool_calls"].as_array().and_then(|c| c.iter().find(|c| c["function"]["name"] == name)) {
                if let Some(r) = msgs[i..].iter().find(|r| r["role"] == "tool" && r["tool_call_id"] == call["id"]) {
                    return Some(r["content"].as_str().unwrap_or_default().to_string());
                }
            }
        }
    }
    None
}

#[test]
fn make_me_a_coffee() {
    // SAFETY: before any other thread exists in this test binary's only test.
    unsafe {
        std::env::set_var("ROOM_MCP_TOKEN", "e2e-token");
        std::env::set_var("SUBJECT_MEMORY_EMBEDDINGS", "off");
        std::env::set_var("DEEPSEEK_API_KEY", "scripted");
    }
    let data = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("vesper-e2e-{}", std::process::id()));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(scenario(&data))));
    stop_postgres(&data.join("pg"));
    let _ = std::fs::remove_dir_all(&data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

async fn scenario(data: &std::path::Path) {
    let (llm, log) = brain().await;
    let cluster = CLUSTER.replace("\"stt\", \"--models\"", "\"stt\", \"--fake\", \"could you make me a coffee\", \"--models\"");
    let any = "127.0.0.1:0".parse().unwrap();
    let r = up(
        Opts {
            data: data.to_path_buf(),
            database: None,
            room: any,
            hub: any,
            webhooks: any,
            llm_url: Some(llm),
            exe: env!("CARGO_BIN_EXE_personality").into(),
            tts: vesper_room::tts::Tts::Silent,
            time_scale: 10.0,
            mcp_token: "e2e-token".into(),
        },
        &cluster,
    )
    .await
    .unwrap();
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

async fn wait(what: &str, f: impl Fn() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}
