//! Shared by the end-to-end tests: a scripted OpenAI-compatible brain,
//! launching everything, and waiting.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// Which tool calls (name, arguments) answer a message, in order.
pub type Script = fn(&str) -> Vec<(&'static str, Value)>;

pub type Log = Arc<Mutex<Vec<Value>>>;

/// An OpenAI-compatible endpoint answering with the script.
/// An OpenAI-compatible endpoint answering with the script: the next call
/// after the last message, or "done" when the script is through.
pub async fn brain(script: Script) -> (String, Log) {
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
pub fn tool_result(log: &Log, name: &str) -> Option<String> {
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

/// Runs `f` with fresh env and a private Postgres in the target dir,
/// cleaning up after. Call first thing: it sets process env.
pub fn with_env(name: &str, env: &[(&str, &str)], f: impl AsyncFnOnce(&std::path::Path)) {
    // SAFETY: before any other thread exists (one test per test binary).
    unsafe {
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        // The hub's router model, kept between runs.
        std::env::set_var("SUBNET_MODELS", std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("models"));
    }
    let data = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("vesper-{name}-{}", std::process::id()));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(f(&data))));
    personality::stop_postgres(&data.join("pg"));
    let _ = std::fs::remove_dir_all(&data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// Everything up, with the brain as her mind and a silent voice.
pub async fn launch(data: &std::path::Path, llm: String, cluster: &str) -> personality::Running {
    let any = "127.0.0.1:0".parse().unwrap();
    personality::up(
        personality::Opts {
            data: data.to_path_buf(),
            database: None,
            room: any,
            hub: any,
            webhooks: any,
            llm_url: Some(llm),
            exe: env!("CARGO_BIN_EXE_personality").into(),
            tts: vesper_room::tts::Tts::Silent,
            time_scale: 10.0,
            mcp_token: std::env::var("ROOM_MCP_TOKEN").unwrap(),
        },
        cluster,
    )
    .await
    .unwrap()
}

pub async fn wait(what: &str, f: impl Fn() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}
