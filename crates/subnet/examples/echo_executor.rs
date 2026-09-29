//! A tiny external executor (see DESIGN.md, "Executors"). It answers from
//! rules instead of a model, which makes it handy for tests:
//! - "tool please" → calls `list_types`, then reports what the tool said
//! - "slow"        → streams words slowly (abortable)
//! - "crash"       → exits, to test restarts
//! - otherwise     → "external: <message>"

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn text(id: u64, s: &str) {
    emit(json!({"t":"delta","id":id,"delta":{"content":s}}));
}

#[tokio::main]
async fn main() {
    let aborted: Arc<Mutex<HashSet<u64>>> = Default::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let hello: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(hello["t"], "hello");
    let ty = hello["type"].as_str().unwrap_or_default().to_string();
    while let Ok(Some(line)) = lines.next_line().await {
        let m: Value = serde_json::from_str(&line).unwrap();
        let id = m["id"].as_u64().unwrap();
        if m["t"] == "abort" {
            aborted.lock().unwrap().insert(id);
            continue;
        }
        let last = m["messages"].as_array().and_then(|a| a.last()).cloned().unwrap_or_default();
        let content = last["content"].as_str().unwrap_or_default().to_string();
        if last["role"] == "tool" {
            text(id, &format!("tool said: {content}"));
            emit(json!({"t":"done","id":id}));
        } else if content.contains("tool please") {
            emit(json!({"t":"delta","id":id,"delta":{"tool_calls":[{"index":0,"id":"x1","name":"list_types","arguments":"{}"}]}}));
            emit(json!({"t":"done","id":id}));
        } else if content.contains("crash") {
            std::process::exit(3);
        } else if content.contains("slow") {
            let aborted = aborted.clone();
            tokio::spawn(async move {
                for w in ["one ", "two ", "three ", "four ", "five ", "six ", "seven ", "eight"] {
                    if aborted.lock().unwrap().contains(&id) {
                        return;
                    }
                    text(id, w);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                emit(json!({"t":"done","id":id}));
            });
        } else {
            text(id, &format!("{ty} external: "));
            text(id, &content);
            emit(json!({"t":"done","id":id}));
        }
    }
}
