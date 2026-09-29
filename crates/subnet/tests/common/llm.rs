//! Scripted OpenAI-compatible server. Each agent type (identified by its system
//! prompt) has a queue of replies; each reply sees the request body.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, body::Body, extract::State, response::Response, routing::post};
use futures::StreamExt;
use serde_json::{Value, json};

pub type Reply = Box<dyn Fn(&Value) -> Vec<String> + Send + Sync>;

#[derive(Default)]
struct Inner {
    queues: HashMap<String, VecDeque<Reply>>,
    requests: Vec<Value>,
}

#[derive(Clone)]
pub struct MockLlm {
    inner: Arc<Mutex<Inner>>,
    pub gap: Arc<Mutex<Duration>>,
    pub url: String,
}

impl MockLlm {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let gap = Arc::new(Mutex::new(Duration::ZERO));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", l.local_addr().unwrap());
        let app = Router::new().route("/v1/chat/completions", post(handle)).with_state((inner.clone(), gap.clone()));
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Self { inner, gap, url }
    }

    /// Queue a reply for the agent type with this system prompt.
    pub fn push(&self, system: &str, r: impl Fn(&Value) -> Vec<String> + Send + Sync + 'static) {
        self.inner.lock().unwrap().queues.entry(system.into()).or_default().push_back(Box::new(r));
    }

    pub fn say(&self, system: &str, parts: &[&str]) {
        let chunks = text(parts);
        self.push(system, move |_| chunks.clone());
    }

    pub fn requests(&self) -> Vec<Value> {
        self.inner.lock().unwrap().requests.clone()
    }

    pub fn set_gap(&self, d: Duration) {
        *self.gap.lock().unwrap() = d;
    }
}

type Shared = (Arc<Mutex<Inner>>, Arc<Mutex<Duration>>);

async fn handle(State((inner, gap)): State<Shared>, Json(body): Json<Value>) -> Response {
    let system = body["messages"][0]["content"].as_str().unwrap_or_default().to_string();
    let reply = {
        let mut i = inner.lock().unwrap();
        i.requests.push(body.clone());
        i.queues.get_mut(&system).and_then(VecDeque::pop_front)
    };
    let Some(reply) = reply else {
        return Response::builder().status(500).body(Body::from(format!("no scripted reply for {system:?}"))).unwrap();
    };
    let chunks = reply(&body);
    let gap = *gap.lock().unwrap();
    let s = futures::stream::iter(chunks).then(move |c| async move {
        tokio::time::sleep(gap).await;
        Ok::<_, std::io::Error>(format!("data: {c}\n\n"))
    });
    Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(s)).unwrap()
}

pub fn text(parts: &[&str]) -> Vec<String> {
    let mut v: Vec<String> =
        parts.iter().map(|p| json!({"choices":[{"delta":{"content":p}}]}).to_string()).collect();
    v.push(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}).to_string());
    v.push("[DONE]".into());
    v
}

pub fn tool_call(id: &str, name: &str, args: Value) -> Vec<String> {
    vec![
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]}).to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        "[DONE]".into(),
    ]
}

/// Content of the last `tool` message in a request.
pub fn last_tool_result(body: &Value) -> String {
    body["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}
