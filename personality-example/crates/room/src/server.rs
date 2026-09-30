//! The room server: one shared world, logged-in browsers over WebSocket,
//! her voice, and webhooks into the subnet node.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::tts::{self, Tts};
use crate::users::Users;
use crate::world::{Event, World};

pub struct Config {
    /// The node's webhook base, e.g. `http://127.0.0.1:8790/hooks`; chat,
    /// voice and world events are posted to `<hooks>/chat` etc. None: not
    /// connected (the room still works, Vesper just doesn't hear).
    pub hooks: Option<String>,
    /// Simulation speed; tests run the world faster.
    pub time_scale: f64,
    /// If set, `/mcp` requires `Authorization: Bearer <token>`.
    pub mcp_token: Option<String>,
    pub users: PathBuf,
    pub web: PathBuf,
    pub assets: PathBuf,
    pub tts: Tts,
}

const COOKIE: &str = "vesper_session";
const TICK: Duration = Duration::from_millis(100);
/// Longest push-to-talk utterance (16 kHz s16le).
const MAX_UTTERANCE: usize = 30 * 16_000 * 2;
const KEEP_SPEECHES: usize = 32;
const KEEP_CHAT: usize = 50;

pub struct Room {
    pub cfg: Config,
    pub world: Mutex<World>,
    started: Instant,
    pub users: Users,
    sessions: Mutex<HashMap<String, String>>,
    /// Name -> open connections.
    people: Mutex<BTreeMap<String, usize>>,
    out: broadcast::Sender<Arc<str>>,
    chat: Mutex<VecDeque<Value>>,
    speeches: Mutex<VecDeque<(u64, Arc<Vec<u8>>)>>,
    next_speech: AtomicU64,
    /// Bumped every tick, for tools waiting on the world.
    ticks: watch::Sender<u64>,
    /// Bumped by every move_to, so an older walk knows it was replaced.
    pub walks: AtomicU64,
    http: reqwest::Client,
}

impl Room {
    pub fn new(cfg: Config) -> anyhow::Result<Arc<Self>> {
        let users = Users::open(cfg.users.clone())?;
        Ok(Arc::new(Room {
            cfg,
            world: Mutex::new(World::new()),
            started: Instant::now(),
            users,
            sessions: Mutex::default(),
            people: Mutex::default(),
            out: broadcast::channel(256).0,
            chat: Mutex::default(),
            speeches: Mutex::default(),
            next_speech: AtomicU64::new(1),
            ticks: watch::channel(0).0,
            walks: AtomicU64::new(0),
            http: reqwest::Client::new(),
        }))
    }

    /// World time in seconds.
    pub fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * self.cfg.time_scale
    }

    pub fn people(&self) -> Vec<String> {
        self.people.lock().unwrap().keys().cloned().collect()
    }

    fn broadcast(&self, v: Value) {
        let _ = self.out.send(v.to_string().into());
    }

    fn state(&self) -> Value {
        let w = self.world.lock().unwrap();
        let objects: Vec<Value> = w.objects.iter().map(|o| json!({"id": o.id, "pos": o.pos, "rot": o.rot, "state": o.state})).collect();
        json!({"type": "state", "t": w.t, "vesper": w.vesper, "objects": objects, "people": self.people()})
    }

    fn chat_line(&self, line: Value) {
        {
            let mut c = self.chat.lock().unwrap();
            c.push_back(line.clone());
            while c.len() > KEEP_CHAT {
                c.pop_front();
            }
        }
        let mut msg = line;
        msg["type"] = "chat".into();
        self.broadcast(msg);
    }

    /// Posts to one of the node's webhooks. Failures are logged: people
    /// can still see and chat when the node is down.
    pub async fn hook(&self, path: &str, body: Value) {
        let Some(base) = &self.cfg.hooks else { return };
        let url = format!("{}/{path}", base.trim_end_matches('/'));
        match self.http.post(&url).json(&body).send().await.and_then(|r| r.error_for_status()) {
            Ok(_) => {}
            Err(e) => tracing::warn!("webhook {url}: {e}"),
        }
    }

    /// Waits for the next simulation tick.
    pub async fn tick(&self) {
        let mut rx = self.ticks.subscribe();
        let _ = rx.changed().await;
    }

    /// Runs the simulation. Returns never.
    pub async fn run(self: Arc<Self>) {
        let mut every = tokio::time::interval(TICK);
        loop {
            every.tick().await;
            let events = self.world.lock().unwrap().tick(self.now());
            for e in events {
                if e == Event::CoffeeReady {
                    self.hook("world", json!({"event": "coffee_ready", "text": "The coffee maker beeps: fresh coffee is ready."})).await;
                }
            }
            self.ticks.send_modify(|n| *n += 1);
            self.broadcast(self.state());
        }
    }

    /// She speaks: TTS, then every browser gets the text, the audio and the
    /// envelope. Returns the speech length in seconds.
    pub async fn say(&self, text: &str, to: Option<&str>) -> anyhow::Result<f64> {
        let s = self.cfg.tts.speak(text).await?;
        let id = self.next_speech.fetch_add(1, Ordering::Relaxed);
        {
            let mut sp = self.speeches.lock().unwrap();
            sp.push_back((id, Arc::new(s.wav)));
            while sp.len() > KEEP_SPEECHES {
                sp.pop_front();
            }
        }
        self.world.lock().unwrap().speak(s.secs * self.cfg.time_scale);
        self.chat_line(json!({"from": "vesper", "text": text, "to": to}));
        self.broadcast(json!({"type": "speech", "id": id, "url": format!("/speech/{id}.wav"), "secs": s.secs, "frame": tts::FRAME, "envelope": s.envelope, "text": text}));
        Ok(s.secs)
    }

    fn session(&self, headers: &HeaderMap) -> Option<String> {
        let token = cookie(headers, COOKIE)?;
        self.sessions.lock().unwrap().get(&token).cloned()
    }

    async fn joined(&self, name: &str) {
        let first = {
            let mut p = self.people.lock().unwrap();
            let n = p.entry(name.to_string()).or_default();
            *n += 1;
            *n == 1
        };
        if first {
            self.chat_line(json!({"from": null, "text": format!("{name} came in")}));
            self.hook("world", json!({"event": "joined", "who": name, "text": format!("{name} came into the room.")})).await;
        }
    }

    async fn left(&self, name: &str) {
        let last = {
            let mut p = self.people.lock().unwrap();
            let n = p.get_mut(name).map(|n| {
                *n -= 1;
                *n
            });
            if n == Some(0) {
                p.remove(name);
            }
            n == Some(0)
        };
        if last {
            self.chat_line(json!({"from": null, "text": format!("{name} left")}));
            self.hook("world", json!({"event": "left", "who": name, "text": format!("{name} left the room.")})).await;
        }
    }

    pub async fn chat_from(&self, name: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let text: String = text.chars().take(2000).collect();
        self.chat_line(json!({"from": name, "text": text}));
        self.hook("chat", json!({"from": name, "text": text})).await;
    }

    pub async fn voice_from(&self, name: &str, pcm: &[u8]) {
        // Under a quarter second is a click, not speech.
        if pcm.len() < 16_000 / 2 {
            return;
        }
        let wav = match tts::pcm_to_wav(pcm, 16_000) {
            Ok(w) => w,
            Err(e) => return tracing::warn!("voice from {name}: {e}"),
        };
        self.chat_line(json!({"from": name, "text": "(spoke)", "voice": true}));
        let audio = json!({"$blob": {"base64": base64::engine::general_purpose::STANDARD.encode(wav), "mime": "audio/wav"}});
        self.hook("voice", json!({"from": name, "audio": audio})).await;
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

#[derive(Deserialize)]
struct Login {
    name: String,
    password: String,
}

async fn login(State(room): State<Arc<Room>>, Json(l): Json<Login>) -> Response {
    let users = &room.users;
    // argon2 is deliberately slow; keep it off the async threads.
    let checked = tokio::task::block_in_place(|| users.check(&l.name, &l.password));
    let Some(name) = checked else {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "wrong name or password"}))).into_response();
    };
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    room.sessions.lock().unwrap().insert(token.clone(), name.clone());
    let c = format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
    ([(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap())], Json(json!({"name": name}))).into_response()
}

async fn logout(State(room): State<Arc<Room>>, headers: HeaderMap) -> Response {
    if let Some(t) = cookie(&headers, COOKIE) {
        room.sessions.lock().unwrap().remove(&t);
    }
    let c = format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap())], Json(json!({}))).into_response()
}

async fn me(State(room): State<Arc<Room>>, headers: HeaderMap) -> Response {
    match room.session(&headers) {
        Some(name) => Json(json!({"name": name})).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn speech(State(room): State<Arc<Room>>, headers: HeaderMap, Path(file): Path<String>) -> Response {
    if room.session(&headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let id: Option<u64> = file.strip_suffix(".wav").and_then(|s| s.parse().ok());
    let wav = id.and_then(|id| room.speeches.lock().unwrap().iter().find(|(i, _)| *i == id).map(|(_, w)| w.clone()));
    match wav {
        Some(w) => ([(header::CONTENT_TYPE, "audio/wav")], (*w).clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws(State(room): State<Arc<Room>>, headers: HeaderMap, up: WebSocketUpgrade) -> Response {
    match room.session(&headers) {
        Some(name) => up.on_upgrade(move |socket| connection(room, name, socket)),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// One browser: state and speech out; chat and push-to-talk in.
/// Text frames: `{"type": "chat", "text"}`, `{"type": "voice_start"}`,
/// `{"type": "voice_end"}`, `{"type": "voice_cancel"}`; binary frames:
/// 16 kHz mono s16le audio between voice_start and voice_end.
async fn connection(room: Arc<Room>, name: String, mut socket: WebSocket) {
    let mut rx = room.out.subscribe();
    let hello = json!({"type": "hello", "you": name, "chat": room.chat.lock().unwrap().iter().cloned().collect::<Vec<_>>()});
    if socket.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    room.joined(&name).await;
    let mut utterance: Option<Vec<u8>> = None;
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(msg) => if socket.send(Message::text(msg.to_string())).await.is_err() { break },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    match v["type"].as_str() {
                        Some("chat") => room.chat_from(&name, v["text"].as_str().unwrap_or_default()).await,
                        Some("voice_start") => utterance = Some(vec![]),
                        Some("voice_cancel") => utterance = None,
                        Some("voice_end") => if let Some(pcm) = utterance.take() { room.voice_from(&name, &pcm).await },
                        _ => tracing::debug!("{name}: unknown message {t}"),
                    }
                }
                Some(Ok(Message::Binary(b))) => if let Some(u) = &mut utterance {
                    if u.len() + b.len() <= MAX_UTTERANCE { u.extend_from_slice(&b) }
                },
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
    room.left(&name).await;
}

pub fn router(room: Arc<Room>) -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let web = ServeDir::new(&room.cfg.web).fallback(ServeFile::new(room.cfg.web.join("index.html")));
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/ws", get(ws))
        .route("/speech/{file}", get(speech))
        .nest_service("/assets", ServeDir::new(&room.cfg.assets))
        .fallback_service(web)
        .with_state(room.clone())
        .merge(crate::mcp::router(room))
}

/// Serves the room and runs the world until the listener fails.
pub async fn serve(room: Arc<Room>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    tokio::spawn(room.clone().run());
    let app = router(room);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
