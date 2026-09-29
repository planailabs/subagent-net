//! One MCP server (an `mcp` block of the cluster) running on a node.

use std::collections::HashMap;

use axum::http::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest, ContentBlock, ServerResult, Tool};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use subnet_cluster::McpDef;
use subnet_core::chat::ToolDef;
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ()>;

pub struct McpHost {
    pub name: String,
    pub id: String,
    client: Client,
    tools: HashMap<String, Tool>,
}

/// `"$VAR"` reads the node's environment; anything else is a literal.
pub fn env_value(v: &str) -> Result<String, String> {
    match v.strip_prefix('$') {
        Some(var) => std::env::var(var).map_err(|e| format!("env {var}: {e}")),
        None => Ok(v.to_string()),
    }
}

impl McpHost {
    /// Starts (stdio) or connects to (HTTP) the server and lists its tools.
    pub async fn connect(name: &str, id: &str, def: &McpDef) -> Result<Self, String> {
        let client = match (&def.command, &def.url) {
            (Some(cmd), None) => {
                let (prog, args) = cmd.split_first().ok_or("empty command")?;
                let mut env = vec![];
                for (k, v) in &def.env {
                    env.push((k.clone(), env_value(v)?));
                }
                let t = TokioChildProcess::new(tokio::process::Command::new(prog).configure(|c| {
                    c.args(args).envs(env).kill_on_drop(true);
                }))
                .map_err(|e| format!("starting {prog:?}: {e}"))?;
                ().serve(t).await.map_err(|e| e.to_string())?
            }
            (None, Some(url)) => {
                let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.clone());
                if let Some(c) = &def.credential {
                    let v = std::env::var(&c.env).map_err(|e| format!("env {}: {e}", c.env))?;
                    let mut h = HashMap::new();
                    h.insert(
                        HeaderName::try_from(c.header.as_str()).map_err(|e| e.to_string())?,
                        HeaderValue::try_from(format!("{}{v}", c.prefix)).map_err(|e| e.to_string())?,
                    );
                    cfg = cfg.custom_headers(h);
                }
                ().serve(StreamableHttpClientTransport::from_config(cfg)).await.map_err(|e| e.to_string())?
            }
            _ => return Err("needs exactly one of command or url".into()),
        };
        let tools = client.list_all_tools().await.map_err(|e| e.to_string())?;
        let tools = tools.into_iter().map(|t| (t.name.to_string(), t)).collect::<HashMap<_, _>>();
        tracing::info!(mcp = %id, tools = tools.len(), "mcp server connected");
        Ok(Self { name: name.to_string(), id: id.to_string(), client, tools })
    }

    /// The server's tools, unprefixed.
    pub fn defs(&self) -> Vec<ToolDef> {
        let mut v: Vec<_> = self
            .tools
            .iter()
            .map(|(name, t)| ToolDef {
                name: name.clone(),
                description: t.description.as_deref().unwrap_or_default().to_string(),
                parameters: Value::Object((*t.input_schema).clone()),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Calls a tool. On `abort` the server is sent `notifications/cancelled`
    /// and `None` is returned.
    pub async fn call(&self, tool: &str, args: Value, abort: &CancellationToken) -> Option<Result<String, String>> {
        if !self.tools.contains_key(tool) {
            return Some(Err(format!("mcp {} has no tool {tool:?}", self.name)));
        }
        let arguments = match args {
            Value::Object(o) => o,
            Value::Null => Default::default(),
            _ => return Some(Err("arguments must be a JSON object".into())),
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(arguments);
        let req = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let mut handle = match self.client.peer().send_cancellable_request(req, PeerRequestOptions::no_options()).await {
            Ok(h) => h,
            Err(e) => return Some(Err(e.to_string())),
        };
        let resp = tokio::select! {
            r = &mut handle.rx => r,
            _ = abort.cancelled() => {
                if let Err(e) = handle.cancel(Some("aborted by pause".into())).await {
                    tracing::warn!(tool, error = %e, "sending mcp cancellation failed");
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

/// Parses a model's JSON argument string.
pub fn parse_args(args: &str) -> Result<Value, String> {
    let args = if args.trim().is_empty() { "{}" } else { args };
    match serde_json::from_str::<Value>(args) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err("arguments must be a JSON object".into()),
        Err(e) => Err(format!("bad arguments: {e}")),
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

    #[test]
    fn args_must_be_objects() {
        assert!(parse_args("").unwrap().is_object());
        assert!(parse_args("[1]").is_err());
        assert!(parse_args("{nope").unwrap_err().starts_with("bad arguments"));
    }
}
