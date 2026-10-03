//! Senses on a node: a source (command, stream, webhook, timer, file watch)
//! feeding a pipeline of stages (commands, CEL filters and maps). Final
//! events go to the hub; binary streams stay on the node (or are relayed).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{Value, json};
use subnet_cluster::{SenseDef, SourceKind, Stage as StageDef};
use subnet_switchboard::Stage;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Frames kept per stream subscriber before the oldest are dropped.
pub const STREAM_FRAMES: usize = 256;
const FRAME: usize = 4096;

/// What senses report to the node.
#[derive(Debug, Clone, PartialEq)]
pub enum SenseOut {
    Event { sense: String, data: Value },
    Status { sense: String, error: Option<String> },
}

enum Item {
    Event(Value),
    Bytes(Bytes),
}

pub struct Senses {
    out: mpsc::Sender<SenseOut>,
    streams: Mutex<HashMap<String, broadcast::Sender<Bytes>>>,
    webhooks: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    running: Mutex<HashMap<String, (SenseDef, CancellationToken)>>,
}

impl Senses {
    pub fn new(out: mpsc::Sender<SenseOut>) -> Arc<Self> {
        Arc::new(Self { out, streams: Default::default(), webhooks: Default::default(), running: Default::default() })
    }

    /// Whether a webhook at `path` is taking events yet (its sense has started).
    pub fn has_webhook(&self, path: &str) -> bool {
        self.webhooks.lock().unwrap().contains_key(path)
    }

    /// A stream's channel on this node (created on first use by either side).
    pub fn stream(&self, name: &str) -> broadcast::Sender<Bytes> {
        self.streams.lock().unwrap().entry(name.to_string()).or_insert_with(|| broadcast::channel(STREAM_FRAMES).0).clone()
    }

    /// Starts new or changed senses and stops removed ones.
    pub fn configure(self: &Arc<Self>, senses: &indexmap::IndexMap<String, SenseDef>) {
        let mut running = self.running.lock().unwrap();
        running.retain(|name, (def, stop)| {
            let keep = senses.get(name) == Some(def);
            if !keep {
                tracing::info!(sense = %name, "stopping sense");
                stop.cancel();
            }
            keep
        });
        for (name, def) in senses {
            if running.contains_key(name) {
                continue;
            }
            let stop = CancellationToken::new();
            running.insert(name.clone(), (def.clone(), stop.clone()));
            tracing::info!(sense = %name, "starting sense");
            tokio::spawn(self.clone().run(name.clone(), def.clone(), stop));
        }
    }

    async fn status(&self, sense: &str, error: Option<String>) {
        if let Some(e) = &error {
            tracing::warn!(sense, error = %e, "sense problem");
        }
        let _ = self.out.send(SenseOut::Status { sense: sense.into(), error }).await;
    }

    async fn run(self: Arc<Self>, name: String, def: SenseDef, stop: CancellationToken) {
        let kind = match def.source.kind() {
            Ok(k) => k,
            Err(e) => return self.status(&name, Some(e)).await,
        };
        let (tx, mut rx) = mpsc::channel::<Item>(256);
        let _guard = stop.clone().drop_guard();
        match kind {
            SourceKind::ExecStream(cmd, _) => {
                // The sense *is* the stream: its bytes never become events.
                let bus = self.stream(&name);
                let me = self.clone();
                let (cmd, stop2, n) = (cmd.to_vec(), stop.clone(), name.clone());
                tokio::spawn(async move { me.exec_source(&n, &cmd, Out::Stream(bus), stop2).await });
                self.status(&name, None).await;
                stop.cancelled().await;
                return;
            }
            SourceKind::Exec(cmd) => {
                let (me, cmd, stop2, n) = (self.clone(), cmd.to_vec(), stop.clone(), name.clone());
                tokio::spawn(async move { me.exec_source(&n, &cmd, Out::Items(tx), stop2).await });
            }
            SourceKind::Subscribe(from) => {
                let mut sub = self.stream(from).subscribe();
                let stop2 = stop.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop2.cancelled() => return,
                            b = sub.recv() => match b {
                                Ok(b) => { if tx.send(Item::Bytes(b)).await.is_err() { return } }
                                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(dropped = n, "stream subscriber lagging"),
                                Err(broadcast::error::RecvError::Closed) => return,
                            }
                        }
                    }
                });
            }
            SourceKind::Webhook(w) => {
                let (wtx, mut wrx) = mpsc::channel(256);
                self.webhooks.lock().unwrap().insert(w.path.clone(), wtx);
                let (me, path, stop2) = (self.clone(), w.path.clone(), stop.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop2.cancelled() => break,
                            v = wrx.recv() => match v {
                                Some(v) => { if tx.send(Item::Event(v)).await.is_err() { break } }
                                None => break,
                            }
                        }
                    }
                    me.webhooks.lock().unwrap().remove(&path);
                });
            }
            SourceKind::Timer(t) => {
                let (t, stop2) = (t.clone(), stop.clone());
                tokio::spawn(async move { timer_source(t, tx, stop2).await });
            }
            SourceKind::File(f) => match file_source(f, tx, stop.clone()) {
                Ok(()) => {}
                Err(e) => return self.status(&name, Some(e)).await,
            },
        }
        // Stages, in order.
        for (sn, st) in &def.stage {
            let (next_tx, next_rx) = mpsc::channel::<Item>(256);
            match stage_task(&name, sn, st, rx, next_tx, stop.clone()) {
                Ok(()) => rx = next_rx,
                Err(e) => return self.status(&name, Some(format!("stage {sn:?}: {e}"))).await,
            }
        }
        self.status(&name, None).await;
        loop {
            let item = tokio::select! {
                _ = stop.cancelled() => return,
                i = rx.recv() => i,
            };
            match item {
                Some(Item::Event(data)) => {
                    if self.out.try_send(SenseOut::Event { sense: name.clone(), data }).is_err() {
                        tracing::warn!(sense = %name, "hub link busy or down, event dropped");
                    }
                }
                Some(Item::Bytes(_)) => tracing::warn!(sense = %name, "raw bytes reached the end of the pipeline, dropped"),
                None => return,
            }
        }
    }

    /// Runs `cmd`, restarting it (with backoff) when it exits, until stopped.
    async fn exec_source(&self, sense: &str, cmd: &[String], out: Out, stop: CancellationToken) {
        let mut backoff = Duration::from_millis(500);
        loop {
            let started = tokio::time::Instant::now();
            match self.exec_once(cmd, &out, &stop).await {
                Ok(()) if stop.is_cancelled() => return,
                Ok(()) => self.status(sense, Some(format!("{:?} exited", cmd[0]))).await,
                Err(e) => self.status(sense, Some(e)).await,
            }
            if started.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_millis(500);
            }
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn exec_once(&self, cmd: &[String], out: &Out, stop: &CancellationToken) -> Result<(), String> {
        let (prog, args) = cmd.split_first().ok_or("empty command")?;
        let mut child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting {prog:?}: {e}"))?;
        let mut stdout = child.stdout.take().unwrap();
        match out {
            Out::Stream(bus) => {
                let mut buf = vec![0u8; FRAME];
                loop {
                    let n = tokio::select! {
                        _ = stop.cancelled() => return Ok(()),
                        n = stdout.read(&mut buf) => n.map_err(|e| e.to_string())?,
                    };
                    if n == 0 {
                        return Ok(());
                    }
                    // No subscribers is fine: frames are simply not kept.
                    let _ = bus.send(Bytes::copy_from_slice(&buf[..n]));
                }
            }
            Out::Items(tx) => {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    let line = tokio::select! {
                        _ = stop.cancelled() => return Ok(()),
                        l = lines.next_line() => l.map_err(|e| e.to_string())?,
                    };
                    let Some(line) = line else { return Ok(()) };
                    if line.trim().is_empty() {
                        continue;
                    }
                    if tx.send(Item::Event(line_to_event(&line))).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// `POST /hooks/<path>` delivers the body (JSON, else `{"body": text}`)
    /// to the webhook sense listening on `/<path>`.
    pub fn webhook_router(self: &Arc<Self>) -> Router {
        webhook_router(vec![self.clone()])
    }
}

enum Out {
    Stream(broadcast::Sender<Bytes>),
    Items(mpsc::Sender<Item>),
}

/// Replaces inline blobs (`{"$blob": {"base64": …, "mime": …}}`) in event data
/// with `blob:<sha256>` references (an image with its marker, `[image
/// blob:<sha256> image/png 800x600]`, so a model that sees gets it); returns
/// the blobs to upload first.
pub fn extract_blobs(v: &mut Value) -> Vec<(String, String, String)> {
    let mut out = vec![];
    walk(v, &mut out);
    out
}

fn walk(v: &mut Value, out: &mut Vec<(String, String, String)>) {
    if let Some(inner) = v.get("$blob")
        && v.as_object().is_some_and(|o| o.len() == 1)
        && let Some(b64) = inner["base64"].as_str()
    {
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
            let h = crate::hub::blobs::hash(&bytes);
            let mime = inner["mime"].as_str().unwrap_or("application/octet-stream").to_string();
            let dims = mime.starts_with("image/").then(|| image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok()?.into_dimensions().ok()).flatten();
            out.push((h.clone(), mime.clone(), b64.to_string()));
            *v = Value::String(match dims {
                Some((w, ht)) => {
                    super::vision::remember(&h, &mime, &bytes);
                    super::vision::marker(&h, &mime, w, ht)
                }
                None => format!("blob:{h}"),
            });
            return;
        }
    }
    match v {
        Value::Array(a) => a.iter_mut().for_each(|x| walk(x, out)),
        Value::Object(o) => o.values_mut().for_each(|x| walk(x, out)),
        _ => {}
    }
}

/// A line of command output: JSON if it parses, else `{"line": …}`.
fn line_to_event(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|_| json!({ "line": line }))
}

/// One webhook endpoint for several nodes' senses (e.g. `subnet dev`).
pub fn webhook_router(senses: Vec<Arc<Senses>>) -> Router {
    Router::new().route("/hooks/{*path}", post(webhook)).with_state(Arc::new(senses))
}

async fn webhook(State(all): State<Arc<Vec<Arc<Senses>>>>, Path(path): Path<String>, body: Bytes) -> StatusCode {
    let key = format!("/{path}");
    let Some(tx) = all.iter().find_map(|s| s.webhooks.lock().unwrap().get(&key).cloned()) else {
        return StatusCode::NOT_FOUND;
    };
    let v = serde_json::from_slice(&body).unwrap_or_else(|_| json!({ "body": String::from_utf8_lossy(&body) }));
    match tx.try_send(v) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn timer_source(t: subnet_cluster::Timer, tx: mpsc::Sender<Item>, stop: CancellationToken) {
    let cron = t.cron.as_deref().and_then(|c| c.parse::<croner::Cron>().ok());
    let mut tick = 0u64;
    loop {
        let wait = match (&t.every, &cron) {
            (Some(d), _) => d.0,
            (None, Some(c)) => {
                let now = chrono::Utc::now();
                match c.find_next_occurrence(&now, false) {
                    Ok(next) => (next - now).to_std().unwrap_or_default(),
                    Err(_) => return,
                }
            }
            _ => return,
        };
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        tick += 1;
        let at = chrono::Utc::now().timestamp_millis();
        if tx.send(Item::Event(json!({"tick": tick, "at": at}))).await.is_err() {
            return;
        }
    }
}

fn file_source(f: &subnet_cluster::FileWatch, tx: mpsc::Sender<Item>, stop: CancellationToken) -> Result<(), String> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let glob = f
        .glob
        .as_deref()
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| e.to_string())?;
    let (ntx, mut nrx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |r: notify::Result<notify::Event>| {
        let _ = ntx.send(r);
    })
    .map_err(|e| e.to_string())?;
    watcher.watch(std::path::Path::new(&f.path), RecursiveMode::Recursive).map_err(|e| format!("{}: {e}", f.path))?;
    tokio::spawn(async move {
        let _watcher = watcher; // lives as long as this task
        loop {
            let r = tokio::select! {
                _ = stop.cancelled() => return,
                r = nrx.recv() => r,
            };
            let Some(Ok(ev)) = r else { continue };
            let kind = match ev.kind {
                EventKind::Create(_) => "create",
                EventKind::Modify(_) => "modify",
                EventKind::Remove(_) => "remove",
                _ => continue,
            };
            for p in ev.paths {
                let name = p.file_name().map(std::path::Path::new).unwrap_or(&p);
                if glob.as_ref().is_some_and(|g| !g.is_match(name)) {
                    continue;
                }
                let v = json!({"path": p.to_string_lossy(), "kind": kind});
                if tx.send(Item::Event(v)).await.is_err() {
                    return;
                }
            }
        }
    });
    Ok(())
}

fn stage_task(
    sense: &str,
    name: &str,
    def: &StageDef,
    mut rx: mpsc::Receiver<Item>,
    tx: mpsc::Sender<Item>,
    stop: CancellationToken,
) -> Result<(), String> {
    if let Some(cmd) = &def.exec {
        let (sense, cmd) = (sense.to_string(), cmd.clone());
        tokio::spawn(async move { exec_stage(&sense, &cmd, rx, tx, stop).await });
        return Ok(());
    }
    let mut stage = match (&def.filter, &def.map) {
        (Some(f), None) => Stage::filter(f)?,
        (None, Some(m)) => Stage::map(m)?,
        _ => return Err("needs exactly one of exec, filter, map".into()),
    };
    let (sense, name) = (sense.to_string(), name.to_string());
    tokio::spawn(async move {
        loop {
            let item = tokio::select! {
                _ = stop.cancelled() => return,
                i = rx.recv() => i,
            };
            let out = match item {
                None => return,
                Some(Item::Bytes(_)) => {
                    tracing::warn!(%sense, stage = %name, "CEL stages take events, not stream bytes");
                    continue;
                }
                Some(Item::Event(v)) => match stage.apply(v) {
                    Ok(Some(v)) => Item::Event(v),
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(%sense, stage = %name, error = %e, "stage failed on an event");
                        continue;
                    }
                },
            };
            if tx.send(out).await.is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// A long-running command: items in on stdin (events as JSON lines, stream
/// bytes raw), events out as JSON lines. Restarted if it exits.
async fn exec_stage(sense: &str, cmd: &[String], mut rx: mpsc::Receiver<Item>, tx: mpsc::Sender<Item>, stop: CancellationToken) {
    let mut backoff = Duration::from_millis(500);
    while !stop.is_cancelled() {
        let (prog, args) = cmd.split_first().expect("validated: command not empty");
        let child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(sense, error = %e, "starting stage {prog:?} failed");
                tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(backoff) => {} }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                item = rx.recv() => {
                    let Some(item) = item else { return };
                    let r = match item {
                        Item::Event(v) => stdin.write_all(format!("{v}\n").as_bytes()).await,
                        Item::Bytes(b) => stdin.write_all(&b).await,
                    };
                    if r.is_err() { break }
                }
                line = lines.next_line() => match line {
                    Ok(Some(l)) if !l.trim().is_empty() => {
                        if tx.send(Item::Event(line_to_event(&l))).await.is_err() { return }
                    }
                    Ok(Some(_)) => {}
                    _ => break,
                },
            }
        }
        tracing::warn!(sense, "stage {prog:?} exited, restarting");
        tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(backoff) => {} }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_blobs_become_references() {
        let mut v = json!({"clip": {"$blob": {"base64": "aGVsbG8=", "mime": "text/plain"}}, "n": 1, "list": [{"$blob": {"base64": "eA=="}}]});
        let blobs = extract_blobs(&mut v);
        assert_eq!(blobs.len(), 2);
        assert_eq!(v["clip"], format!("blob:{}", crate::hub::blobs::hash(b"hello")));
        assert_eq!(blobs[0].1, "text/plain");
        assert_eq!(blobs[1].1, "application/octet-stream");
        assert_eq!(v["n"], 1);
        // An image is a marker, which a model that sees is given.
        let mut png = std::io::Cursor::new(vec![]);
        image::DynamicImage::new_rgb8(3, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
        use base64::Engine;
        let mut v = json!({"photo": {"$blob": {"base64": base64::engine::general_purpose::STANDARD.encode(png.get_ref()), "mime": "image/png"}}});
        extract_blobs(&mut v);
        let h = crate::hub::blobs::hash(png.get_ref());
        assert_eq!(v["photo"], format!("[image blob:{h} image/png 3x2]"));
        assert_eq!(super::super::vision::markers(&v.to_string()), [(h, "image/png".to_string())]);
    }

    #[test]
    fn lines_become_events() {
        assert_eq!(line_to_event(r#"{"a":1}"#), json!({"a":1}));
        assert_eq!(line_to_event("hello"), json!({"line":"hello"}));
    }
}
