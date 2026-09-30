//! The room server over HTTP: login, browsers over WebSocket, webhooks out,
//! and the world MCP.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use vesper_room::server::{self, Config, Room};
use vesper_room::tts::Tts;

type Hooks = Arc<Mutex<Vec<(String, Value)>>>;

/// A stand-in for the node's webhook listener.
async fn hooks() -> (String, Hooks) {
    let got: Hooks = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().route(
        "/hooks/{path}",
        axum::routing::post(move |axum::extract::Path(p): axum::extract::Path<String>, axum::Json(v): axum::Json<Value>| {
            let g = g.clone();
            async move { g.lock().unwrap().push((p, v)) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hooks", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

struct T {
    base: String,
    hooks: Hooks,
    room: Arc<Room>,
}

async fn start() -> T {
    start_at(20.0).await
}

async fn start_at(time_scale: f64) -> T {
    let (hook_url, got) = hooks().await;
    let dir = std::env::temp_dir().join(format!("vesper-room-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
    let room = Room::new(Config {
        hooks: Some(hook_url),
        time_scale,
        mcp_token: Some("letmein".into()),
        users: dir.join("users.json"),
        web: dir.join("web"),
        assets: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets"),
        tts: Tts::Silent,
    })
    .unwrap();
    room.users.set("alice", "wonderland").unwrap();
    room.users.set("bob", "builder1").unwrap();
    std::fs::create_dir_all(dir.join("web")).unwrap();
    std::fs::write(dir.join("web/index.html"), "<title>room</title>").unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve(room.clone(), l));
    T { base, hooks: got, room }
}

impl T {
    async fn login(&self, name: &str, pw: &str) -> Option<String> {
        let r = reqwest::Client::new().post(format!("{}/api/login", self.base)).json(&json!({"name": name, "password": pw})).send().await.unwrap();
        if !r.status().is_success() {
            return None;
        }
        let c = r.headers()["set-cookie"].to_str().unwrap();
        Some(c.split(';').next().unwrap().to_string())
    }

    async fn ws(&self, cookie: &str) -> Ws {
        let mut req = format!("{}/ws", self.base.replace("http", "ws")).into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    async fn hook(&self, path: &str, f: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            if let Some((_, v)) = self.hooks.lock().unwrap().iter().find(|(p, v)| p == path && f(v)) {
                return v.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no {path} hook; got {:?}", self.hooks.lock().unwrap());
    }

    async fn mcp(&self, token: &str) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, String> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", self.base)).auth_header(token);
        ().serve(StreamableHttpClientTransport::from_config(cfg)).await.map_err(|e| e.to_string())
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next(ws: &mut Ws, kind: &str, f: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let m = tokio::time::timeout_at(deadline, ws.next()).await.unwrap_or_else(|_| panic!("no {kind} message")).unwrap().unwrap();
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == kind && f(&v) {
                return v;
            }
        }
    }
}

async fn call(mcp: &rmcp::service::RunningService<rmcp::RoleClient, ()>, tool: &'static str, args: Value) -> (bool, String) {
    let mut p = CallToolRequestParams::new(tool);
    p.arguments = args.as_object().cloned();
    let r = mcp.call_tool(p).await.unwrap();
    (r.is_error == Some(true), r.content[0].as_text().unwrap().text.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn login_and_sessions() {
    let t = start().await;
    assert!(t.login("alice", "nope").await.is_none());
    assert!(t.login("mallory", "wonderland").await.is_none());
    let c = t.login("Alice", "wonderland").await.unwrap();
    let http = reqwest::Client::new();
    let me: Value = http.get(format!("{}/api/me", t.base)).header("cookie", &c).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["name"], "alice");
    assert_eq!(http.get(format!("{}/api/me", t.base)).send().await.unwrap().status(), 401);
    assert_eq!(http.get(format!("{}/api/me", t.base)).header("cookie", "vesper_session=forged").send().await.unwrap().status(), 401);
    // No WebSocket without a session.
    let req = format!("{}/ws", t.base.replace("http", "ws")).into_client_request().unwrap();
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    http.post(format!("{}/api/logout", t.base)).header("cookie", &c).send().await.unwrap();
    assert_eq!(http.get(format!("{}/api/me", t.base)).header("cookie", &c).send().await.unwrap().status(), 401);
    // Static files: the web app (with a fallback to index.html) and assets.
    assert!(http.get(format!("{}/some/route", t.base)).send().await.unwrap().text().await.unwrap().contains("<title>room"));
    let glb = http.get(format!("{}/assets/vesper.glb", t.base)).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(&glb[..4], b"glTF");
}

#[tokio::test(flavor = "multi_thread")]
async fn browsers_chat_and_talk() {
    let t = start().await;
    let (ca, cb) = (t.login("alice", "wonderland").await.unwrap(), t.login("bob", "builder1").await.unwrap());
    let mut a = t.ws(&ca).await;
    assert_eq!(next(&mut a, "hello", |_| true).await["you"], "alice");
    let joined = t.hook("world", |v| v["event"] == "joined").await;
    assert_eq!(joined["who"], "alice");
    let st = next(&mut a, "state", |v| v["people"] == json!(["alice"])).await;
    assert_eq!(st["vesper"]["pose"], "stand");
    assert_eq!(st["objects"].as_array().unwrap().len(), 9);

    let mut b = t.ws(&cb).await;
    next(&mut a, "state", |v| v["people"] == json!(["alice", "bob"])).await;
    a.send(Message::text(json!({"type": "chat", "text": "  hi Vesper  "}).to_string())).await.unwrap();
    let line = next(&mut b, "chat", |v| v["from"] == "alice").await;
    assert_eq!(line["text"], "hi Vesper");
    assert_eq!(t.hook("chat", |_| true).await, json!({"from": "alice", "text": "hi Vesper"}));

    // Push-to-talk: one second of audio becomes a WAV blob for the voice hook.
    b.send(Message::text(json!({"type": "voice_start"}).to_string())).await.unwrap();
    for _ in 0..4 {
        b.send(Message::binary(vec![0u8; 8000])).await.unwrap();
    }
    b.send(Message::text(json!({"type": "voice_end"}).to_string())).await.unwrap();
    let v = t.hook("voice", |_| true).await;
    assert_eq!(v["from"], "bob");
    assert_eq!(v["audio"]["$blob"]["mime"], "audio/wav");
    let wav = base64::engine::general_purpose::STANDARD.decode(v["audio"]["$blob"]["base64"].as_str().unwrap()).unwrap();
    let r = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
    assert_eq!((r.spec().sample_rate, r.duration()), (16_000, 16_000));
    // A click is ignored; a cancelled utterance isn't sent.
    b.send(Message::text(json!({"type": "voice_start"}).to_string())).await.unwrap();
    b.send(Message::binary(vec![0u8; 100])).await.unwrap();
    b.send(Message::text(json!({"type": "voice_end"}).to_string())).await.unwrap();

    // A later browser gets the recent chat.
    let mut a2 = t.ws(&ca).await;
    let hello = next(&mut a2, "hello", |_| true).await;
    assert!(hello["chat"].as_array().unwrap().iter().any(|l| l["text"] == "hi Vesper"));
    drop(b);
    assert_eq!(t.hook("world", |v| v["event"] == "left").await["who"], "bob");
    assert_eq!(t.hooks.lock().unwrap().iter().filter(|(p, _)| p == "voice").count(), 1);
    // alice still has a2 open: closing one of two connections isn't leaving.
    drop(a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!t.hooks.lock().unwrap().iter().any(|(_, v)| v["event"] == "left" && v["who"] == "alice"));
    assert_eq!(t.room.people(), ["alice"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_world_mcp_makes_coffee() {
    let t = start().await;
    assert!(t.mcp("wrong").await.is_err(), "the MCP endpoint wants the token");
    let mcp = t.mcp("letmein").await.unwrap();
    let mut tools: Vec<String> = mcp.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    tools.sort();
    assert_eq!(tools, ["gesture", "interact", "look_around", "look_at", "move_to", "say"]);
    let c = t.login("alice", "wonderland").await.unwrap();
    let mut ws = t.ws(&c).await;

    let (_, seen) = call(&mcp, "look_around", json!({})).await;
    let seen: Value = serde_json::from_str(&seen).unwrap();
    assert_eq!(seen["people"], json!(["alice"]));
    let (err, msg) = call(&mcp, "interact", json!({"object": "coffee_maker", "action": "brew"})).await;
    assert!(err && msg.contains("move_to"), "{msg}");
    let (err, msg) = call(&mcp, "move_to", json!({"target": "coffee_maker"})).await;
    assert!(!err && msg.starts_with("arrived at the coffee_maker"), "{msg}");
    next(&mut ws, "state", |v| v["vesper"]["pose"] == "stand" && v["vesper"]["pos"][0].as_f64().unwrap() < -2.0).await;
    assert_eq!(call(&mcp, "interact", json!({"object": "coffee_maker", "action": "brew"})).await.1, "brewing; ready in 20 s");
    call(&mcp, "interact", json!({"object": "mug", "action": "pick_up"})).await;
    // 20 world seconds at 20x.
    t.hook("world", |v| v["event"] == "coffee_ready").await;
    assert_eq!(call(&mcp, "interact", json!({"object": "coffee_maker", "action": "pour"})).await.1, "poured a mug of coffee");
    let (err, msg) = call(&mcp, "move_to", json!({"target": "alice"})).await;
    assert!(!err, "{msg}");
    call(&mcp, "gesture", json!({"name": "wave"})).await;
    assert!(call(&mcp, "gesture", json!({"name": "dab"})).await.0);
    let (err, msg) = call(&mcp, "say", json!({"text": "Here's your coffee.", "to": "alice"})).await;
    assert!(!err && msg.starts_with("said it"), "{msg}");
    let chat = next(&mut ws, "chat", |v| v["from"] == "vesper").await;
    assert_eq!(chat["to"], "alice");
    let speech = next(&mut ws, "speech", |_| true).await;
    assert_eq!(speech["text"], "Here's your coffee.");
    assert!(!speech["envelope"].as_array().unwrap().is_empty());
    let url = format!("{}{}", t.base, speech["url"].as_str().unwrap());
    let http = reqwest::Client::new();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    let wav = http.get(&url).header("cookie", &c).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert!(call(&mcp, "move_to", json!({"target": {"x": 99, "z": 0}})).await.0);
    assert!(call(&mcp, "look_at", json!({"target": "window"})).await.1 == "turned");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_walk_replaces_the_old_one() {
    let t = start_at(1.0).await;
    let mcp = Arc::new(t.mcp("letmein").await.unwrap());
    let m = mcp.clone();
    let first = tokio::spawn(async move { call(&m, "move_to", json!({"target": "bookshelf"})).await });
    while !t.room.world.lock().unwrap().walking() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, second) = call(&mcp, "move_to", json!({"target": "coffee_maker"})).await;
    assert!(second.starts_with("arrived at the coffee_maker"), "{second}");
    assert!(first.await.unwrap().1.starts_with("stopped on the way to the bookshelf"));
}
