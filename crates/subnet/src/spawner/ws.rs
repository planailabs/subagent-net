//! Spawner side of the hub WebSocket, with reconnect.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use subnet_core::proto::ToSpawner;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::Spawner;

/// Keeps the spawner connected to the hub. Returns only if the hub rejects
/// us (bad token, bad hello), which retrying can't fix.
pub async fn run(spawner: Arc<Spawner>, url: &str) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1);
                if let Some(reason) = session(&spawner, ws).await {
                    anyhow::bail!("hub rejected spawner: {reason}");
                }
                tracing::warn!("disconnected from hub");
            }
            Err(e) => tracing::warn!(error = %e, %url, "cannot reach hub"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One connection. Returns the rejection reason if the hub refused us.
async fn session(spawner: &Spawner, ws: Ws) -> Option<String> {
    let (mut sink, mut stream) = ws.split();
    let hello = serde_json::to_string(&spawner.hello()).unwrap();
    if sink.send(Message::Text(hello.into())).await.is_err() {
        return None;
    }
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let pump = async {
        loop {
            tokio::select! {
                out = out_rx.recv() => {
                    let Some(m) = out else { return None };
                    if sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await.is_err() {
                        return None;
                    }
                }
                inc = stream.next() => match inc {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ToSpawner>(&t) {
                        Ok(ToSpawner::Rejected { reason }) => return Some(reason),
                        Ok(m) => { let _ = in_tx.send(m); }
                        Err(e) => tracing::warn!(error = %e, "unparseable hub message"),
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                    Some(Ok(_)) => {}
                },
            }
        }
    };
    // When the socket goes, `serve` sees its inbox close and drops every agent.
    tokio::select! {
        r = pump => r,
        _ = spawner.serve(in_rx, out_tx) => None,
    }
}
