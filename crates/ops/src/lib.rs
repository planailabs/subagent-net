//! Typed operation registry. An operation is defined once (name, summary,
//! role, argument and result types, optional REST route) and served as:
//! REST/RPC endpoints with an OpenAPI document ([`http`]), MCP tools
//! ([`mcp`]) and CLI subcommands ([`cli`]); [`client`] calls it over HTTP.

pub mod cli;
pub mod client;
pub mod http;
pub mod mcp;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a caller may do. Ordered: each role includes the ones below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

impl ErrorKind {
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::BadRequest => 400,
            ErrorKind::Unauthorized => 401,
            ErrorKind::Forbidden => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::Unavailable => 503,
            ErrorKind::Internal => 500,
        }
    }

    fn from_status(s: u16) -> Self {
        match s {
            400 | 422 => ErrorKind::BadRequest,
            401 => ErrorKind::Unauthorized,
            403 => ErrorKind::Forbidden,
            404 => ErrorKind::NotFound,
            409 => ErrorKind::Conflict,
            503 => ErrorKind::Unavailable,
            _ => ErrorKind::Internal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct OpError {
    pub kind: ErrorKind,
    pub message: String,
}

impl OpError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn bad(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// An operation's definition. Implemented by a marker type per operation.
pub trait Op: 'static {
    /// snake_case; also the MCP tool name. The CLI uses it kebab-cased.
    const NAME: &'static str;
    const SUMMARY: &'static str;
    const ROLE: Role;
    /// Optional REST route, e.g. `(Method::Post, "/v1/agents/{id}/pause")`.
    /// Path parameters are fields of `Args`.
    const HTTP: Option<(Method, &'static str)> = None;
    type Args: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    type Out: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

/// Everything about an operation except its handler.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpMeta {
    pub name: &'static str,
    pub summary: &'static str,
    pub role: Role,
    pub http: Option<(Method, &'static str)>,
    /// JSON schema of the arguments (may contain `$defs`).
    pub args: Value,
    /// JSON schema of the result.
    pub out: Value,
}

impl OpMeta {
    pub fn of<O: Op>() -> Self {
        Self {
            name: O::NAME,
            summary: O::SUMMARY,
            role: O::ROLE,
            http: O::HTTP,
            args: serde_json::to_value(schemars::schema_for!(O::Args)).unwrap(),
            out: serde_json::to_value(schemars::schema_for!(O::Out)).unwrap(),
        }
    }

    /// Names of `{param}` segments in the REST path, in order.
    pub fn path_params(&self) -> Vec<&'static str> {
        let Some((_, path)) = self.http else { return vec![] };
        path.split('/').filter_map(|s| s.strip_prefix('{')?.strip_suffix('}')).collect()
    }
}

/// The identity a call is made as.
pub trait Caller: Clone + Send + Sync + 'static {
    fn role(&self) -> Role;
}

type Handler<C> = Arc<dyn Fn(C, Value) -> BoxFuture<'static, Result<Value, OpError>> + Send + Sync>;

struct Entry<C> {
    meta: OpMeta,
    handler: Handler<C>,
}

/// Operations with their handlers, keyed by name.
pub struct Registry<C> {
    ops: BTreeMap<&'static str, Entry<C>>,
}

impl<C: Caller> Default for Registry<C> {
    fn default() -> Self {
        Self { ops: BTreeMap::new() }
    }
}

impl<C: Caller> Registry<C> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the handler of operation `O`. Panics on a duplicate name: that
    /// is a programming error caught by any test that builds the registry.
    pub fn add<O, F, Fut>(&mut self, f: F) -> &mut Self
    where
        O: Op,
        F: Fn(C, O::Args) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O::Out, OpError>> + Send + 'static,
    {
        let f = Arc::new(f);
        let handler: Handler<C> = Arc::new(move |c, v| {
            let f = f.clone();
            Box::pin(async move {
                let args: O::Args = parse_args(v)?;
                let out = f(c, args).await?;
                serde_json::to_value(out).map_err(|e| OpError::internal(e.to_string()))
            })
        });
        let prev = self.ops.insert(O::NAME, Entry { meta: OpMeta::of::<O>(), handler });
        assert!(prev.is_none(), "duplicate op {}", O::NAME);
        self
    }

    pub fn metas(&self) -> impl Iterator<Item = &OpMeta> {
        self.ops.values().map(|e| &e.meta)
    }

    pub fn meta(&self, name: &str) -> Option<&OpMeta> {
        self.ops.get(name).map(|e| &e.meta)
    }

    /// Runs an operation as `caller`, checking its role first.
    pub async fn call(&self, caller: C, name: &str, args: Value) -> Result<Value, OpError> {
        let e = self.ops.get(name).ok_or_else(|| OpError::not_found(format!("no operation {name:?}")))?;
        if caller.role() < e.meta.role {
            return Err(OpError::forbidden(format!("{name} needs role {:?}", e.meta.role)));
        }
        (e.handler)(caller, args).await
    }
}

/// `null` counts as `{}` so argument-less operations need no body.
fn parse_args<T: DeserializeOwned>(v: Value) -> Result<T, OpError> {
    let v = if v.is_null() { Value::Object(Default::default()) } else { v };
    serde_json::from_value(v).map_err(|e| OpError::bad(format!("bad arguments: {e}")))
}

/// Argument type of operations without arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    #[derive(Clone)]
    pub struct Who(pub Role);
    impl Caller for Who {
        fn role(&self) -> Role {
            self.0
        }
    }

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    pub struct AddArgs {
        /// First number.
        pub a: i64,
        pub b: i64,
    }
    pub struct Add;
    impl Op for Add {
        const NAME: &'static str = "add";
        const SUMMARY: &'static str = "Add two numbers.";
        const ROLE: Role = Role::Viewer;
        type Args = AddArgs;
        type Out = i64;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    pub enum Mode {
        Soft,
        Hard,
    }
    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    pub struct KickArgs {
        /// Who to kick.
        pub id: String,
        pub mode: Mode,
        #[serde(default)]
        pub tree: bool,
        #[serde(default)]
        pub tags: Vec<String>,
        #[serde(default)]
        pub times: Option<u32>,
    }
    #[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
    pub struct Kicked {
        pub id: String,
        pub mode: Mode,
        pub tree: bool,
        pub tags: Vec<String>,
        pub times: Option<u32>,
    }
    pub struct Kick;
    impl Op for Kick {
        const NAME: &'static str = "kick_thing";
        const SUMMARY: &'static str = "Kick a thing.";
        const ROLE: Role = Role::Operator;
        const HTTP: Option<(Method, &'static str)> = Some((Method::Post, "/v1/things/{id}/kick"));
        type Args = KickArgs;
        type Out = Kicked;
    }

    pub struct List;
    impl Op for List {
        const NAME: &'static str = "list_things";
        const SUMMARY: &'static str = "List things.";
        const ROLE: Role = Role::Viewer;
        const HTTP: Option<(Method, &'static str)> = Some((Method::Get, "/v1/things"));
        type Args = NoArgs;
        type Out = Vec<String>;
    }

    pub struct Nuke;
    impl Op for Nuke {
        const NAME: &'static str = "nuke";
        const SUMMARY: &'static str = "Admins only.";
        const ROLE: Role = Role::Admin;
        type Args = NoArgs;
        type Out = String;
    }

    pub fn registry() -> Registry<Who> {
        let mut r = Registry::new();
        r.add::<Add, _, _>(|_, a| async move { Ok(a.a + a.b) });
        r.add::<Kick, _, _>(|_, a| async move {
            if a.id == "missing" {
                return Err(OpError::not_found("no such thing"));
            }
            Ok(Kicked { id: a.id, mode: a.mode, tree: a.tree, tags: a.tags, times: a.times })
        });
        r.add::<List, _, _>(|_, _| async { Ok(vec!["x".to_string(), "y".to_string()]) });
        r.add::<Nuke, _, _>(|_, _| async { Ok("boom".to_string()) });
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn calls_and_checks_roles() {
        let r = registry();
        assert_eq!(r.call(Who(Role::Viewer), "add", json!({"a":2,"b":3})).await.unwrap(), json!(5));
        let e = r.call(Who(Role::Operator), "nuke", Value::Null).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Forbidden);
        assert_eq!(r.call(Who(Role::Admin), "nuke", Value::Null).await.unwrap(), json!("boom"));
        assert_eq!(r.call(Who(Role::Admin), "nope", Value::Null).await.unwrap_err().kind, ErrorKind::NotFound);
        let e = r.call(Who(Role::Viewer), "add", json!({"a":"x"})).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
    }

    #[test]
    fn meta_has_schemas_and_path_params() {
        let r = registry();
        let m = r.meta("kick_thing").unwrap();
        assert_eq!(m.path_params(), vec!["id"]);
        assert_eq!(m.args["properties"]["id"]["description"], "Who to kick.");
        assert!(r.meta("add").unwrap().path_params().is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate op")]
    fn duplicate_names_panic() {
        let mut r = registry();
        r.add::<Add, _, _>(|_, a| async move { Ok(a.a) });
    }
}
