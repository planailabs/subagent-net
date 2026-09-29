//! MCP tool servers of an agent type, connected once per spawner.

use std::collections::HashMap;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ContentBlock, ServerResult, Tool,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use subnet_core::chat::ToolDef;
use tokio_util::sync::CancellationToken;

use super::config::McpServerConfig;

type Client = RunningService<RoleClient, ()>;

pub struct McpTools {
    clients: Vec<Client>,
    /// Tool name → (client index, tool).
    tools: HashMap<String, (usize, Tool)>,
}

/// `"$VAR"` reads the spawner's environment; anything else is a literal.
fn env_value(v: &str) -> anyhow::Result<String> {
    match v.strip_prefix('$') {
        Some(var) => std::env::var(var).map_err(|e| anyhow::anyhow!("env {var}: {e}")),
        None => Ok(v.to_string()),
    }
}

impl McpTools {
    pub fn empty() -> Self {
        Self { clients: vec![], tools: HashMap::new() }
    }

    /// Connects every server and lists its tools. Tool names must be unique
    /// across servers and built-ins (`reserved`).
    pub async fn connect(cfgs: &[McpServerConfig], reserved: &[String]) -> anyhow::Result<Self> {
        let mut me = Self::empty();
        for cfg in cfgs {
            let client = match (&cfg.command, &cfg.url) {
                (Some(cmd), None) => {
                    let mut env = vec![];
                    for (k, v) in &cfg.env {
                        env.push((k.clone(), env_value(v)?));
                    }
                    let t = TokioChildProcess::new(tokio::process::Command::new(cmd).configure(|c| {
                        c.args(&cfg.args).envs(env);
                    }))?;
                    ().serve(t).await?
                }
                (None, Some(url)) => ().serve(StreamableHttpClientTransport::from_uri(url.as_str())).await?,
                _ => anyhow::bail!("mcp server {:?} needs exactly one of command or url", cfg.name),
            };
            let tools = client.list_all_tools().await?;
            let idx = me.clients.len();
            for t in tools {
                let name = t.name.to_string();
                anyhow::ensure!(!reserved.contains(&name), "mcp server {:?}: tool {name:?} shadows a built-in", cfg.name);
                anyhow::ensure!(!me.tools.contains_key(&name), "mcp server {:?}: duplicate tool {name:?}", cfg.name);
                me.tools.insert(name, (idx, t));
            }
            tracing::info!(server = %cfg.name, tools = me.tools.len(), "mcp server connected");
            me.clients.push(client);
        }
        Ok(me)
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        let mut v: Vec<_> = self
            .tools
            .iter()
            .map(|(name, (_, t))| ToolDef {
                name: name.clone(),
                description: t.description.as_deref().unwrap_or_default().to_string(),
                parameters: Value::Object((*t.input_schema).clone()),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Calls a tool. On `abort` the server is sent `notifications/cancelled`
    /// and `None` is returned.
    pub async fn call(&self, name: &str, args: &str, abort: &CancellationToken) -> Option<Result<String, String>> {
        let Some((idx, _)) = self.tools.get(name) else {
            return Some(Err(format!("unknown tool {name:?}")));
        };
        let args = if args.trim().is_empty() { "{}" } else { args };
        let arguments = match serde_json::from_str::<Value>(args) {
            Ok(Value::Object(o)) => o,
            Ok(_) => return Some(Err("arguments must be a JSON object".into())),
            Err(e) => return Some(Err(format!("bad arguments: {e}"))),
        };
        let mut params = CallToolRequestParams::new(name.to_string());
        params.arguments = Some(arguments);
        let req = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let peer = self.clients[*idx].peer();
        let mut handle = match peer.send_cancellable_request(req, PeerRequestOptions::no_options()).await {
            Ok(h) => h,
            Err(e) => return Some(Err(e.to_string())),
        };
        let resp = tokio::select! {
            r = &mut handle.rx => r,
            _ = abort.cancelled() => {
                if let Err(e) = handle.cancel(Some("aborted by pause".into())).await {
                    tracing::warn!(tool = name, error = %e, "sending mcp cancellation failed");
                }
                return None;
            }
        };
        Some(match resp {
            Ok(Ok(ServerResult::CallToolResult(r))) => {
                let text = render(&r.content, r.structured_content.as_ref());
                if r.is_error == Some(true) { Err(text) } else { Ok(text) }
            }
            Ok(Ok(other)) => Err(format!("unexpected mcp response: {other:?}")),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("mcp server went away".into()),
        })
    }
}

/// Tool output for the model: text blocks as-is, anything else as JSON.
fn render(content: &[ContentBlock], structured: Option<&Value>) -> String {
    if content.is_empty()
        && let Some(s) = structured
    {
        return s.to_string();
    }
    content
        .iter()
        .map(|c| match c.as_text() {
            Some(t) => t.text.clone(),
            None => serde_json::to_string(c).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values() {
        assert_eq!(env_value("lit").unwrap(), "lit");
        assert!(env_value("$SUBNET_SURELY_UNSET").is_err());
    }

    #[test]
    fn renders_text_and_structured() {
        assert_eq!(render(&[ContentBlock::text("a"), ContentBlock::text("b")], None), "a\nb");
        assert_eq!(render(&[], Some(&serde_json::json!({"x":1}))), r#"{"x":1}"#);
    }
}
