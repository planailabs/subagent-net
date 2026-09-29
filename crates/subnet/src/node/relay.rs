//! Node side of the stream relay: publishes local streams other nodes
//! subscribe to, and feeds remote streams into local subscribers.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::senses::Senses;
use crate::hub::Hub;
use crate::hub::relay::{decode, encode};

/// How this node reaches the hub's relay.
#[derive(Clone)]
pub enum RelayLink {
    /// Over `<hub>/streams`.
    Ws { hub: String, name: String, token: Option<String> },
    /// In the same process (`subnet dev`, tests).
    Local(Arc<Hub>),
}

impl RelayLink {
    /// Identity, to notice when a node moved to another hub.
    pub fn key(&self) -> String {
        match self {
            RelayLink::Ws { hub, .. } => hub.clone(),
            RelayLink::Local(_) => "local".into(),
        }
    }
}

/// Pumps until `stop`: `out` streams from this node to the relay, `inn`
/// streams from the relay into this node.
pub async fn run(link: RelayLink, senses: Arc<Senses>, out: Vec<String>, inn: Vec<String>, stop: CancellationToken) {
    match link {
        RelayLink::Local(hub) => {
            for name in out {
                let mut sub = senses.stream(&name).subscribe();
                let (hub, stop) = (hub.clone(), stop.clone());
                tokio::spawn(async move { pump(&mut sub, stop, |b| hub.relay.publish(&name, b)).await });
            }
            for name in inn {
                let mut sub = hub.relay.channel(&name).subscribe();
                let local = senses.stream(&name);
                let stop = stop.clone();
                tokio::spawn(async move {
                    pump(&mut sub, stop, |b| {
                        let _ = local.send(b);
                    })
                    .await
                });
            }
            stop.cancelled().await;
        }
        RelayLink::Ws { hub, name, token } => {
            let mut backoff = Duration::from_millis(500);
            while !stop.is_cancelled() {
                let base = hub.trim_end_matches('/').replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
                let mut url = format!("{base}/streams?node={name}&subscribe={}", inn.join(","));
                if let Some(t) = &token {
                    url.push_str(&format!("&token={t}"));
                }
                match tokio_tungstenite::connect_async(url.as_str()).await {
                    Ok((ws, _)) => {
                        backoff = Duration::from_millis(500);
                        session(ws, &senses, &out, &stop).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "stream relay unreachable"),
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn pump(sub: &mut broadcast::Receiver<Bytes>, stop: CancellationToken, mut f: impl FnMut(Bytes)) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            b = sub.recv() => match b {
                Ok(b) => f(b),
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::debug!(dropped = n, "relay lagging"),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn session(ws: Ws, senses: &Senses, out: &[String], stop: &CancellationToken) {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let conn = stop.child_token();
    let _guard = conn.clone().drop_guard();
    for name in out {
        let mut sub = senses.stream(name).subscribe();
        let (tx, conn, name) = (tx.clone(), conn.clone(), name.clone());
        tokio::spawn(async move {
            pump(&mut sub, conn, |b| {
                // Full buffer: drop the frame rather than stall the source.
                let _ = tx.try_send(encode(&name, &b));
            })
            .await
        });
    }
    drop(tx);
    let mut outgoing = !out.is_empty();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            f = rx.recv(), if outgoing => match f {
                Some(f) => if sink.send(Message::Binary(f.into())).await.is_err() { return },
                None => outgoing = false,
            },
            m = stream.next() => match m {
                Some(Ok(Message::Binary(f))) => {
                    if let Some((name, data)) = decode(&f) {
                        let _ = senses.stream(name).send(Bytes::copy_from_slice(data));
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
