use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use futures::StreamExt;
use serde_json::{Map, Value};
use subnet_core::chat::{Accumulator, Message, ToolCall};
use subnet_llm::{Client, Error, ModelConfig};

#[derive(Clone)]
struct Mock {
    /// SSE `data:` payloads to send, in order.
    chunks: Vec<String>,
    /// Status codes to return before succeeding.
    fail_with: Vec<u16>,
    /// Delay between chunks.
    gap: Duration,
    hits: Arc<AtomicUsize>,
    last_body: Arc<std::sync::Mutex<Option<Value>>>,
    last_auth: Arc<std::sync::Mutex<Option<String>>>,
}

impl Mock {
    fn new(chunks: &[&str]) -> Self {
        Self {
            chunks: chunks.iter().map(|s| s.to_string()).collect(),
            fail_with: vec![],
            gap: Duration::ZERO,
            hits: Default::default(),
            last_body: Default::default(),
            last_auth: Default::default(),
        }
    }
}

async fn handler(State(m): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let n = m.hits.fetch_add(1, Ordering::SeqCst);
    *m.last_body.lock().unwrap() = Some(body);
    *m.last_auth.lock().unwrap() = headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
    if let Some(code) = m.fail_with.get(n) {
        return (StatusCode::from_u16(*code).unwrap(), "nope").into_response();
    }
    let gap = m.gap;
    let s = futures::stream::iter(m.chunks.clone()).then(move |c| async move {
        tokio::time::sleep(gap).await;
        Ok::<_, std::io::Error>(format!("data: {c}\n\n"))
    });
    Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(s)).unwrap()
}

async fn serve(m: Mock) -> String {
    let app = Router::new().route("/v1/chat/completions", post(handler)).with_state(m);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/v1")
}

fn client(base_url: String) -> Client {
    Client::new(
        ModelConfig { base_url, model: "test".into(), api_key_env: None, prefill: false, params: Map::new() },
        Some("sk-test".into()),
    )
}

const TEXT: &[&str] = &[
    r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
    r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
    r#"{"choices":[{"delta":{"content":" world"},"finish_reason":"stop"}]}"#,
    r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
    "[DONE]",
];

#[tokio::test]
async fn streams_text_to_completion() {
    let m = Mock::new(TEXT);
    let c = client(serve(m.clone()).await);
    let mut s = c.stream(&[Message::user("hi")], &[]).await.unwrap();
    let mut acc = Accumulator::default();
    while let Some(d) = s.next().await {
        acc.push(&d.unwrap());
    }
    assert_eq!(acc.finish().content.as_deref(), Some("Hello world"));
    assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
    assert_eq!(acc.usage.unwrap().total(), 12);
    assert_eq!(m.last_auth.lock().unwrap().as_deref(), Some("Bearer sk-test"));
    assert_eq!(m.last_body.lock().unwrap().as_ref().unwrap()["model"], "test");
}

#[tokio::test]
async fn streams_tool_calls() {
    let m = Mock::new(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let c = client(serve(m).await);
    let mut acc = Accumulator::default();
    let mut s = c.stream(&[Message::user("hi")], &[]).await.unwrap();
    while let Some(d) = s.next().await {
        acc.push(&d.unwrap());
    }
    assert_eq!(acc.finish().tool_calls, vec![ToolCall::new("c1", "read", r#"{"path":"a"}"#)]);
}

#[tokio::test]
async fn dropping_stream_keeps_partial() {
    let mut m = Mock::new(TEXT);
    m.gap = Duration::from_millis(50);
    let c = client(serve(m).await);
    let mut s = c.stream(&[Message::user("hi")], &[]).await.unwrap();
    let mut acc = Accumulator::default();
    // Take two chunks, then abort, as a hard pause does.
    for _ in 0..2 {
        acc.push(&s.next().await.unwrap().unwrap());
    }
    drop(s);
    assert_eq!(acc.partial().content.as_deref(), Some("Hello"));
}

#[tokio::test]
async fn retries_on_503_then_succeeds() {
    let mut m = Mock::new(TEXT);
    m.fail_with = vec![503, 429];
    let c = client(serve(m.clone()).await);
    let mut s = c.stream(&[Message::user("hi")], &[]).await.unwrap();
    while s.next().await.is_some() {}
    assert_eq!(m.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn does_not_retry_client_errors() {
    let mut m = Mock::new(TEXT);
    m.fail_with = vec![400];
    let c = client(serve(m.clone()).await);
    let e = c.stream(&[Message::user("hi")], &[]).await.err().unwrap();
    assert!(matches!(e, Error::Status { status: 400, .. }));
    assert_eq!(m.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_error_mid_stream_surfaces() {
    let m = Mock::new(&[r#"{"choices":[{"delta":{"content":"a"}}]}"#, r#"{"error":{"message":"boom"}}"#]);
    let c = client(serve(m).await);
    let items: Vec<_> = c.stream(&[Message::user("hi")], &[]).await.unwrap().collect().await;
    assert!(items[0].is_ok());
    assert!(matches!(items[1], Err(Error::Provider(_))));
}
