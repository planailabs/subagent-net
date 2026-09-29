//! Client side of the hub's user surfaces: MCP calls and the event stream.

use std::collections::HashMap;

use anyhow::Context;
use axum::http::{HeaderName, HeaderValue};
use futures::StreamExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use subnet_core::addr::AgentId;

use crate::hub::mcp::AS_HEADER;

pub struct Remote {
    client: RunningService<RoleClient, ()>,
}

impl Remote {
    /// `base` is the hub's HTTP address, e.g. `http://127.0.0.1:7700`.
    /// `who` is `user` or a client name.
    pub async fn connect(base: &str, token: Option<&str>, who: &str) -> anyhow::Result<Self> {
        let mut headers = HashMap::new();
        headers.insert(HeaderName::from_static(AS_HEADER), HeaderValue::from_str(who)?);
        let mut cfg = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", base.trim_end_matches('/')))
            .custom_headers(headers);
        if let Some(t) = token {
            cfg = cfg.auth_header(t);
        }
        let client = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.context("connecting to hub")?;
        Ok(Self { client })
    }

    /// Closes the MCP session cleanly.
    pub async fn close(self) -> anyhow::Result<()> {
        self.client.cancel().await?;
        Ok(())
    }

    /// Calls a hub tool and parses its JSON answer.
    pub async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value> {
        let mut p = CallToolRequestParams::new(tool.to_string());
        p.arguments = match args {
            Value::Object(o) => Some(o),
            Value::Null => None,
            other => anyhow::bail!("arguments must be an object, got {other}"),
        };
        let r = self.client.call_tool(p).await?;
        let text: String = r.content.iter().filter_map(ContentBlock::as_text).map(|t| t.text.as_str()).collect();
        anyhow::ensure!(r.is_error != Some(true), "{text}");
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

/// Streams committed events (as JSON) from the hub until the connection ends.
pub async fn tail(
    base: &str,
    token: Option<&str>,
    agent: Option<AgentId>,
    mut on: impl FnMut(Value),
) -> anyhow::Result<()> {
    let ws_base = base.replacen("http", "ws", 1);
    let mut url = format!("{}/events?", ws_base.trim_end_matches('/'));
    if let Some(a) = agent {
        url.push_str(&format!("agent={a}&"));
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
