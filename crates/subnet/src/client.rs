//! Client side of the hub's user surfaces: operations and the event stream.

use anyhow::Context;
use futures::StreamExt;
use serde_json::Value;
use subnet_core::addr::AgentId;

pub use subnet_ops::client::Client;

/// Streams notices (as JSON) from the hub until the connection ends: all of
/// them, one agent's, or (`tree`) an agent's and its descendants'.
pub async fn tail(
    base: &str,
    token: Option<&str>,
    agent: Option<AgentId>,
    tree: bool,
    mut on: impl FnMut(Value),
) -> anyhow::Result<()> {
    let ws_base = base.split(',').next().unwrap_or_default().trim().replacen("http", "ws", 1);
    let mut url = format!("{}/v1/events/ws?", ws_base.trim_end_matches('/'));
    if let Some(a) = agent {
        url.push_str(&format!("{}={a}&", if tree { "tree" } else { "agent" }));
    }
    if let Some(t) = token {
        url.push_str(&format!("token={t}"));
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.context("connecting to hub events")?;
    while let Some(m) = ws.next().await {
        if let tokio_tungstenite::tungstenite::Message::Text(t) = m? {
            on(serde_json::from_str(&t)?);
        }
    }
    Ok(())
}
