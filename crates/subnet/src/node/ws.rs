//! Node side of the hub WebSocket, with reconnect across several hub URLs.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::Node;
use crate::wire::ToNode;

/// `http(s)://host` or `ws(s)://host` → the node endpoint.
pub fn node_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    let b = b.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    if b.ends_with("/node") { b } else { format!("{b}/node") }
}

/// Keeps the node connected to one of the hubs (comma-separated). Returns only
/// if a hub rejects us (bad token, undeclared node), which retrying can't fix.
pub async fn run(node: Arc<Node>, hubs: &str) -> anyhow::Result<()> {
    let urls: Vec<String> = hubs.split(',').filter(|s| !s.trim().is_empty()).map(node_url).collect();
    anyhow::ensure!(!urls.is_empty(), "no hub URL");
    let mut backoff = Duration::from_secs(1);
    let mut i = 0;
    loop {
        let url = &urls[i % urls.len()];
        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((ws, _)) => {
                let base = url.trim_end_matches("/node").to_string();
                node.set_relay_link(super::relay::RelayLink::Ws { hub: base, name: node.name.clone(), token: node.token.clone() });
                backoff = Duration::from_secs(1);
                match session(&node, ws).await {
                    Some(reason) if reason.starts_with("not the leader") => tracing::info!(%url, "{reason}"),
                    Some(reason) => anyhow::bail!("hub rejected node: {reason}"),
                    None => tracing::warn!(%url, "disconnected from hub"),
                }
            }
            Err(e) => tracing::warn!(error = %e, %url, "cannot reach hub"),
        }
        i += 1;
        // Try every hub quickly before backing off.
        if i % urls.len() == 0 {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One connection. Returns the rejection reason if the hub refused us.
async fn session(node: &Node, ws: Ws) -> Option<String> {
    let (mut sink, mut stream) = ws.split();
    let hello = serde_json::to_string(&node.hello()).unwrap();
    if sink.send(Message::Text(hello.into())).await.is_err() {
        return None;
    }
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let pump = async {
        loop {
            tokio::select! {
                out = out_rx.recv() => {
                    let m = out?; // channel closed: plain disconnect
                    if sink.send(Message::Text(serde_json::to_string(&m).unwrap().into())).await.is_err() {
                        return None;
                    }
                }
                inc = stream.next() => match inc {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ToNode>(&t) {
                        Ok(ToNode::Rejected { reason }) => return Some(reason),
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
        _ = node.serve(in_rx, out_tx) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(node_url("http://h:7700"), "ws://h:7700/node");
        assert_eq!(node_url("https://h/"), "wss://h/node");
        assert_eq!(node_url("ws://h/node"), "ws://h/node");
    }
}
