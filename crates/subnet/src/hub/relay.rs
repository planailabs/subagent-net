//! Stream relay: binary streams between senses on different nodes.
//! One WebSocket per node (`/streams`), frames multiplexed by stream name:
//! `[name length: u8][name][bytes]`. Never logged; slow subscribers lose the
//! oldest frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::broadcast;

use super::Hub;

/// Frames buffered per subscriber.
pub const FRAMES: usize = 256;

#[derive(Default)]
pub struct Relay {
    streams: Mutex<HashMap<String, broadcast::Sender<Bytes>>>,
}

impl Relay {
    pub fn channel(&self, name: &str) -> broadcast::Sender<Bytes> {
        self.streams.lock().unwrap().entry(name.to_string()).or_insert_with(|| broadcast::channel(FRAMES).0).clone()
    }

    pub fn publish(&self, name: &str, data: Bytes) {
        // Nobody listening is fine; the frame is dropped.
        let _ = self.channel(name).send(data);
    }
}

pub fn encode(name: &str, data: &[u8]) -> Vec<u8> {
    let n = name.len().min(255);
    let mut v = Vec::with_capacity(1 + n + data.len());
    v.push(n as u8);
    v.extend_from_slice(&name.as_bytes()[..n]);
    v.extend_from_slice(data);
    v
}

pub fn decode(frame: &[u8]) -> Option<(&str, &[u8])> {
    let n = *frame.first()? as usize;
    let name = std::str::from_utf8(frame.get(1..1 + n)?).ok()?;
    Some((name, &frame[1 + n..]))
}

#[derive(Deserialize)]
pub struct StreamsQuery {
    node: String,
    #[serde(default)]
    token: Option<String>,
    /// Comma-separated streams this node wants.
    #[serde(default)]
    subscribe: String,
}

pub async fn streams_ws(State(hub): State<Arc<Hub>>, Query(q): Query<StreamsQuery>, ws: WebSocketUpgrade) -> Response {
    if !hub.node_ok(&q.node, q.token.as_deref()) {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    let subs: Vec<String> = q.subscribe.split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    ws.on_upgrade(move |ws| serve(hub, q.node, subs, ws))
}

async fn serve(hub: Arc<Hub>, node: String, subs: Vec<String>, mut ws: WebSocket) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(FRAMES);
    // One forwarder per subscribed stream; they end with the connection.
    let stop = tokio_util::sync::CancellationToken::new();
    let _guard = stop.clone().drop_guard();
    for name in subs {
        let mut sub = hub.relay.channel(&name).subscribe();
        let (tx, stop) = (tx.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    f = sub.recv() => match f {
                        Ok(b) => { if tx.send(encode(&name, &b)).await.is_err() { return } }
                        Err(broadcast::error::RecvError::Lagged(n)) => tracing::debug!(stream = %name, dropped = n, "relay subscriber lagging"),
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        });
    }
    drop(tx);
    tracing::info!(%node, "stream relay connected");
    // False once every forwarder is gone (or there were none).
    let mut outgoing = true;
    loop {
        tokio::select! {
            out = rx.recv(), if outgoing => match out {
                Some(f) => if ws.send(Message::Binary(f.into())).await.is_err() { break },
                None => outgoing = false,
            },
            inc = ws.recv() => match inc {
                Some(Ok(Message::Binary(f))) => {
                    if let Some((name, data)) = decode(&f) {
                        hub.relay.publish(name, Bytes::copy_from_slice(data));
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
    tracing::info!(%node, "stream relay disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let f = encode("mic", b"\x00\x01pcm");
        assert_eq!(decode(&f), Some(("mic", &b"\x00\x01pcm"[..])));
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[5, b'a']), None);
    }
}
