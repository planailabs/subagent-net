//! One MCP server (an `mcp` block of the cluster) running on a node.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Per-tenant MCP servers (`per_tenant = true`): one process per tenant,
/// started on its first call and stopped when idle. The base instance (the
/// `default_tenant`'s) lists the tools and is kept with the node's others.
#[derive(Default)]
pub struct Tenants {
    defs: std::sync::RwLock<HashMap<String, (String, McpDef)>>,
    running: tokio::sync::Mutex<HashMap<(String, String), (Arc<McpHost>, Instant)>>,
}

/// How long an unused tenant instance lives.
pub const TENANT_IDLE: Duration = Duration::from_secs(600);

impl Tenants {
    /// Takes the node's MCP servers; instances of servers it no longer runs
    /// (or that changed) are stopped.
    pub async fn configure(&self, mcps: &[subnet_cluster::NodeMcp]) {
        let defs: HashMap<String, (String, McpDef)> = mcps.iter().filter(|m| m.def.per_tenant).map(|m| (m.id.clone(), (m.name.clone(), m.def.clone()))).collect();
        self.running.lock().await.retain(|(id, _), _| defs.contains_key(id));
        *self.defs.write().unwrap() = defs;
    }

    /// The instance for `tenant` when `id` is per-tenant and `tenant` isn't
    /// its default (that one is the base instance); `None` otherwise.
    pub async fn host(&self, id: &str, tenant: Option<&str>) -> Option<Result<Arc<McpHost>, String>> {
        let tenant = tenant?;
        let (name, def) = self.defs.read().unwrap().get(id).cloned()?;
        if def.default_tenant.as_deref() == Some(tenant) {
            return None;
        }
        let mut running = self.running.lock().await;
        let key = (id.to_string(), tenant.to_string());
        if let Some((h, used)) = running.get_mut(&key) {
            *used = Instant::now();
            return Some(Ok(h.clone()));
        }
        tracing::info!(mcp = %id, %tenant, "starting a tenant's mcp server");
        Some(match McpHost::connect(&name, id, &def, Some(tenant)).await {
            Ok(h) => {
                let h = Arc::new(h);
                running.insert(key, (h.clone(), Instant::now()));
                Ok(h)
            }
            Err(e) => Err(format!("starting mcp {id} for tenant {tenant}: {e}")),
        })
    }

    /// Stops instances nobody used for `idle` (and nobody is calling).
    pub async fn sweep(&self, idle: Duration) {
        self.running.lock().await.retain(|(id, tenant), (h, used)| {
            let keep = used.elapsed() < idle || Arc::strong_count(h) > 1;
            if !keep {
                tracing::info!(mcp = %id, %tenant, "stopping an idle tenant's mcp server");
            }
            keep
        });
    }

    /// Tenant instances running now (for tests and status).
    pub async fn running(&self) -> Vec<(String, String)> {
        self.running.lock().await.keys().cloned().collect()
    }
}

/// `"$VAR"` reads the node's environment, `${VAR}` inside a string does
/// too, and `${TENANT}` is the calling agent's tenant (per-tenant servers);
/// anything else is a literal.
pub fn env_value(v: &str, tenant: Option<&str>) -> Result<String, String> {
    let var = |name: &str| -> Result<String, String> {
        match (name, tenant) {
            ("TENANT", Some(t)) => Ok(t.to_string()),
            ("TENANT", None) => Err("${TENANT} needs per_tenant (and a default_tenant for the base instance)".into()),
            _ => std::env::var(name).map_err(|e| format!("env {name}: {e}")),
        }
    };
    if let Some(name) = v.strip_prefix('$')
        && !name.starts_with('{')
    {
        return var(name);
    }
    let mut out = String::new();
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find('}').ok_or_else(|| format!("unclosed ${{ in {v:?}"))? + i;
        out.push_str(&var(&rest[i + 2..end])?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl McpHost {
    /// Starts (stdio) or connects to (HTTP) the server and lists its tools.
    /// `tenant` fills `${TENANT}` (per-tenant servers).
    pub async fn connect(name: &str, id: &str, def: &McpDef, tenant: Option<&str>) -> Result<Self, String> {
        let client = match (&def.command, &def.url) {
            (Some(cmd), None) => {
                let (prog, args) = cmd.split_first().ok_or("empty command")?;
                let mut env = vec![];
                for (k, v) in &def.env {
                    env.push((k.clone(), env_value(v, tenant)?));
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

    /// Calls a tool, with `meta` as the request's `_meta` (who calls). On
    /// `abort` the server is sent `notifications/cancelled` and `None` is
    /// returned.
    pub async fn call(&self, tool: &str, args: Value, meta: Option<&serde_json::Map<String, Value>>, abort: &CancellationToken) -> Option<Result<String, String>> {
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
        if let Some(m) = meta.filter(|m| !m.is_empty()) {
            params.meta = Some(rmcp::model::RequestMetaObject(rmcp::model::MetaObject(m.clone())));
        }
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

/// The `_meta` of an agent's tool calls: who calls (`subnet/agent`), its
/// parent and tenant if it has them. A server serving many agents (one per
/// task, say) tells them apart by it.
pub fn caller_meta(agent: uuid::Uuid, spec: &subnet_core::agent::Spec) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("subnet/agent".into(), Value::String(agent.to_string()));
    if let Some(p) = spec.parent {
        m.insert("subnet/parent".into(), Value::String(p.to_string()));
    }
    if let Some(t) = &spec.tenant {
        m.insert("subnet/tenant".into(), Value::String(t.clone()));
    }
    m
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

/// Tool output for the model: text blocks as-is, images and audio as inline
/// blobs (`{"$blob": {"base64", "mime"}}`, which the agent's node stores and
/// turns into a reference), anything else as JSON.
fn render(content: &[ContentBlock], structured: Option<&Value>) -> String {
    if content.is_empty()
        && let Some(s) = structured
    {
        return s.to_string();
    }
    content
        .iter()
        .map(|c| match c {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::Image(i) => serde_json::json!({"$blob": {"base64": i.data, "mime": i.mime_type}}).to_string(),
            ContentBlock::Audio(a) => serde_json::json!({"$blob": {"base64": a.data, "mime": a.mime_type}}).to_string(),
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values() {
        assert_eq!(env_value("lit", None).unwrap(), "lit");
        assert!(env_value("$SUBNET_SURELY_UNSET", None).is_err());
        assert_eq!(env_value("agents/${TENANT}/id", Some("acme")).unwrap(), "agents/acme/id");
        assert!(env_value("${TENANT}", None).is_err(), "no tenant to fill in");
        assert_eq!(env_value("${PATH}", None).unwrap(), std::env::var("PATH").unwrap());
        assert!(env_value("${OPEN", None).is_err());
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
