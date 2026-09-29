//! MCP front-end: every operation the caller's role allows is a tool.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;

use crate::http::Auth;
use crate::{Caller, ErrorKind, OpError, Registry};

pub struct McpServer<C> {
    reg: Arc<Registry<C>>,
    auth: Auth<C>,
    instructions: Option<String>,
}

impl<C> Clone for McpServer<C> {
    fn clone(&self) -> Self {
        Self { reg: self.reg.clone(), auth: self.auth.clone(), instructions: self.instructions.clone() }
    }
}

/// Streamable-HTTP MCP service; mount it with `nest_service("/mcp", …)`.
pub fn service<C: Caller>(
    reg: Arc<Registry<C>>,
    auth: Auth<C>,
    instructions: Option<String>,
) -> StreamableHttpService<McpServer<C>, LocalSessionManager> {
    let s = McpServer { reg, auth, instructions };
    StreamableHttpService::new(move || Ok(s.clone()), LocalSessionManager::default().into(), StreamableHttpServerConfig::default())
}

impl<C: Caller> McpServer<C> {
    async fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<C, OpError> {
        let headers = ctx.extensions.get::<Parts>().map(|p| p.headers.clone()).unwrap_or_default();
        (self.auth)(headers).await
    }
}

fn protocol_error(e: OpError) -> ErrorData {
    match e.kind {
        ErrorKind::Unauthorized | ErrorKind::Forbidden => ErrorData::invalid_request(e.message, None),
        _ => ErrorData::internal_error(e.message, None),
    }
}

/// A tool input schema: an object schema without the `$schema` marker.
fn input_schema(args: &Value) -> Arc<serde_json::Map<String, Value>> {
    let mut o = args.as_object().cloned().unwrap_or_default();
    o.remove("$schema");
    o.entry("type").or_insert(Value::String("object".into()));
    Arc::new(o)
}

impl<C: Caller> ServerHandler for McpServer<C> {
    fn get_info(&self) -> InitializeResult {
        let mut r = InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
        r.instructions = self.instructions.clone();
        r
    }

    async fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let c = self.caller(&ctx).await.map_err(protocol_error)?;
        let tools = self
            .reg
            .metas()
            .filter(|m| c.role() >= m.role)
            .map(|m| Tool::new(m.name, m.summary, input_schema(&m.args)))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let c = self.caller(&ctx).await.map_err(protocol_error)?;
        let args = req.arguments.map(Value::Object).unwrap_or(Value::Null);
        let r = match self.reg.call(c, &req.name, args).await {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&v).unwrap())]),
            // The caller should see why a tool failed, so this is a tool-level error.
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.message)]),
        };
        Ok(r.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ErrorKind, Role};
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use serde_json::json;
    use std::collections::HashMap;

    fn auth() -> Auth<Who> {
        Arc::new(|h: HeaderMap| {
            Box::pin(async move {
                match h.get("x-role").and_then(|v| v.to_str().ok()) {
                    Some("admin") => Ok(Who(Role::Admin)),
                    Some("view") => Ok(Who(Role::Viewer)),
                    _ => Err(OpError::new(ErrorKind::Unauthorized, "who are you")),
                }
            })
        })
    }

    async fn client(url: &str, role: &'static str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let mut h = HashMap::new();
        h.insert(HeaderName::from_static("x-role"), HeaderValue::from_static(role));
        let cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string()).custom_headers(h);
        ().serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap()
    }

    #[tokio::test]
    async fn tools_follow_roles_and_calls_work() {
        let svc = service(Arc::new(registry()), auth(), Some("test server".into()));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, axum::Router::new().nest_service("/mcp", svc)).await.unwrap() });

        let viewer = client(&url, "view").await;
        let names: Vec<String> = viewer.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["add", "list_things"], "operator/admin tools are hidden");
        let mut p = CallToolRequestParams::new("add");
        p.arguments = json!({"a": 40, "b": 2}).as_object().cloned();
        let r = viewer.call_tool(p).await.unwrap();
        assert_eq!(r.content[0].as_text().unwrap().text, "42");
        let r = viewer.call_tool(CallToolRequestParams::new("nuke")).await.unwrap();
        assert_eq!(r.is_error, Some(true));

        let admin = client(&url, "admin").await;
        assert_eq!(admin.list_all_tools().await.unwrap().len(), 4);
        let t = admin.list_all_tools().await.unwrap().into_iter().find(|t| t.name == "kick_thing").unwrap();
        assert_eq!(t.input_schema["type"], "object");
        assert!(t.input_schema.get("$schema").is_none());
    }
}
