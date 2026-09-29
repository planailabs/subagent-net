//! REST + RPC front-end: `POST /v1/ops/{name}` for every operation, the
//! declared REST routes, `GET /v1/openapi.json` and `GET /v1/docs`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, RawPathParams, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter, get, post};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::{Caller, Method, OpError, OpMeta, Registry};

/// Turns request headers into a caller (or an auth error).
pub type Auth<C> = Arc<dyn Fn(HeaderMap) -> BoxFuture<'static, Result<C, OpError>> + Send + Sync>;

struct Shared<C> {
    reg: Arc<Registry<C>>,
    auth: Auth<C>,
    title: String,
}

impl<C> Clone for Shared<C> {
    fn clone(&self) -> Self {
        Self { reg: self.reg.clone(), auth: self.auth.clone(), title: self.title.clone() }
    }
}

impl IntoResponse for OpError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.kind.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, axum::Json(self)).into_response()
    }
}

pub fn router<C: Caller>(reg: Arc<Registry<C>>, auth: Auth<C>, title: &str) -> Router {
    let mut rest: BTreeMap<&'static str, Vec<(Method, &'static str)>> = BTreeMap::new();
    for m in reg.metas() {
        if let Some((method, path)) = m.http {
            rest.entry(path).or_default().push((method, m.name));
        }
    }
    let mut r = Router::new()
        .route("/v1/ops/{name}", post(rpc::<C>))
        .route("/v1/openapi.json", get(openapi_json::<C>))
        .route("/v1/docs", get(docs::<C>));
    for (path, ops) in rest {
        let mut mr: MethodRouter<Shared<C>> = MethodRouter::new();
        for (method, name) in ops {
            let filter = match method {
                Method::Get => MethodFilter::GET,
                Method::Post => MethodFilter::POST,
                Method::Put => MethodFilter::PUT,
                Method::Delete => MethodFilter::DELETE,
            };
            let h = move |st: State<Shared<C>>, p: RawPathParams, q: Query<HashMap<String, String>>, h: HeaderMap, b: Bytes| {
                rest_call(st, name, p, q, h, b)
            };
            mr = mr.on(filter, h);
        }
        r = r.route(path, mr);
    }
    r.with_state(Shared { reg, auth, title: title.to_string() })
}

async fn run<C: Caller>(st: &Shared<C>, headers: HeaderMap, name: &str, args: Value) -> Response {
    let caller = match (st.auth)(headers).await {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match st.reg.call(caller, name, args).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

fn body_json(b: &Bytes) -> Result<Value, OpError> {
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(b).map_err(|e| OpError::bad(format!("body is not JSON: {e}")))
}

async fn rpc<C: Caller>(State(st): State<Shared<C>>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    match body_json(&body) {
        Ok(args) => run(&st, headers, &name, args).await,
        Err(e) => e.into_response(),
    }
}

async fn rest_call<C: Caller>(
    State(st): State<Shared<C>>,
    name: &'static str,
    params: RawPathParams,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let meta = st.reg.meta(name).expect("route for registered op");
    let mut args = match body_json(&body) {
        Ok(Value::Null) => Map::new(),
        Ok(Value::Object(o)) => o,
        Ok(_) => return OpError::bad("body must be a JSON object").into_response(),
        Err(e) => return e.into_response(),
    };
    for (k, v) in params.iter().chain(query.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
        args.insert(k.to_string(), coerce(&meta.args, k, v));
    }
    run(&st, headers, name, Value::Object(args)).await
}

/// Resolves `#/$defs/X` references inside one schema document.
pub(crate) fn resolve<'a>(root: &'a Value, s: &'a Value) -> &'a Value {
    match s.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/")) {
        Some(name) => root["$defs"].get(name).map_or(s, |d| resolve(root, d)),
        None => s,
    }
}

/// The JSON types a schema allows (`type` may be a string or a list;
/// `anyOf`/`oneOf` branches are merged).
pub(crate) fn types(root: &Value, s: &Value) -> Vec<String> {
    let s = resolve(root, s);
    let mut out = vec![];
    match &s["type"] {
        Value::String(t) => out.push(t.clone()),
        Value::Array(ts) => out.extend(ts.iter().filter_map(Value::as_str).map(String::from)),
        _ => {}
    }
    for k in ["anyOf", "oneOf"] {
        for b in s[k].as_array().into_iter().flatten() {
            out.extend(types(root, b));
        }
    }
    if out.is_empty() && s.get("enum").is_some() {
        out.push("string".into());
    }
    out
}

/// Path and query values are strings; give them the type the schema wants.
fn coerce(schema: &Value, key: &str, raw: &str) -> Value {
    let ts = types(schema, &schema["properties"][key]);
    if ts.iter().any(|t| t == "string") || ts.is_empty() {
        return Value::String(raw.into());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()))
}

async fn openapi_json<C: Caller>(State(st): State<Shared<C>>) -> Response {
    let metas: Vec<_> = st.reg.metas().cloned().collect();
    axum::Json(openapi(&st.title, &metas)).into_response()
}

/// Moves a schema's `$defs` into `components.schemas` and rewrites refs.
fn hoist(mut s: Value, comps: &mut Map<String, Value>) -> Value {
    if let Some(Value::Object(defs)) = s.as_object_mut().and_then(|o| o.remove("$defs")) {
        for (k, v) in defs {
            let v = hoist(v, comps);
            comps.insert(k, v);
        }
    }
    if let Some(o) = s.as_object_mut() {
        o.remove("$schema");
    }
    rewrite_refs(&mut s);
    s
}

fn rewrite_refs(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(r)) = o.get_mut("$ref")
                && let Some(name) = r.strip_prefix("#/$defs/")
            {
                *r = format!("#/components/schemas/{name}");
            }
            o.values_mut().for_each(rewrite_refs);
        }
        Value::Array(a) => a.iter_mut().for_each(rewrite_refs),
        _ => {}
    }
}

fn responses(out: Value) -> Value {
    json!({
        "200": {"description": "OK", "content": {"application/json": {"schema": out}}},
        "default": {"description": "Error", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/OpError"}}}}
    })
}

/// OpenAPI 3.1 document for a set of operations.
pub fn openapi(title: &str, metas: &[OpMeta]) -> Value {
    let mut comps = Map::new();
    comps.insert(
        "OpError".into(),
        json!({"type":"object","required":["kind","message"],"properties":{
            "kind":{"type":"string","enum":["bad_request","unauthorized","forbidden","not_found","conflict","unavailable","internal"]},
            "message":{"type":"string"}}}),
    );
    let mut paths: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for m in metas {
        let args = hoist(m.args.clone(), &mut comps);
        let out = hoist(m.out.clone(), &mut comps);
        paths.entry(format!("/v1/ops/{}", m.name)).or_default().insert(
            "post".into(),
            json!({
                "operationId": m.name,
                "summary": m.summary,
                "tags": ["rpc"],
                "x-role": m.role,
                "requestBody": {"content": {"application/json": {"schema": args}}},
                "responses": responses(out.clone()),
            }),
        );
        let Some((method, path)) = m.http else { continue };
        let path_params = m.path_params();
        let props = args["properties"].as_object().cloned().unwrap_or_default();
        let required: Vec<&str> = args["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut params: Vec<Value> = path_params
            .iter()
            .map(|p| json!({"name": p, "in": "path", "required": true, "schema": props.get(*p).cloned().unwrap_or(json!({}))}))
            .collect();
        let mut op = json!({
            "operationId": format!("{}_rest", m.name),
            "summary": m.summary,
            "tags": ["rest"],
            "x-role": m.role,
            "responses": responses(out),
        });
        if method == Method::Get {
            for (k, v) in props.iter().filter(|(k, _)| !path_params.contains(&k.as_str())) {
                params.push(json!({"name": k, "in": "query", "required": required.contains(&k.as_str()), "schema": v}));
            }
        } else {
            let mut body = args.clone();
            if let Some(o) = body["properties"].as_object_mut() {
                path_params.iter().for_each(|p| {
                    o.remove(*p);
                });
            }
            if let Some(r) = body["required"].as_array_mut() {
                r.retain(|x| !path_params.iter().any(|p| x == p));
            }
            op["requestBody"] = json!({"content": {"application/json": {"schema": body}}});
        }
        op["parameters"] = Value::Array(params);
        paths.entry(path.to_string()).or_default().insert(method.as_str().to_lowercase(), op);
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": title, "version": env!("CARGO_PKG_VERSION")},
        "paths": paths,
        "components": {
            "schemas": comps,
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}}
        },
        "security": [{"bearer": []}],
    })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn docs<C: Caller>(State(st): State<Shared<C>>) -> Response {
    let mut body = String::new();
    for m in st.reg.metas() {
        let route = m.http.map(|(me, p)| format!("{} {p}", me.as_str())).unwrap_or_default();
        body.push_str(&format!(
            "<section id=\"{n}\"><h2>{n}</h2><p class=meta>POST /v1/ops/{n} &nbsp; {r} &nbsp; role: {role:?}</p><p>{s}</p>\
             <details><summary>arguments</summary><pre>{a}</pre></details>\
             <details><summary>result</summary><pre>{o}</pre></details></section>",
            n = m.name,
            r = esc(&route),
            role = m.role,
            s = esc(m.summary),
            a = esc(&serde_json::to_string_pretty(&m.args).unwrap()),
            o = esc(&serde_json::to_string_pretty(&m.out).unwrap()),
        ));
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{t} API</title><style>\
         :root{{color-scheme:dark}}body{{background:#000;color:#ddd;font:14px/1.5 ui-monospace,monospace;max-width:960px;margin:auto;padding:16px}}\
         h1,h2{{color:#fff;font-weight:600}}h2{{border-top:1px solid #333;padding-top:16px}}.meta{{color:#888}}\
         pre{{background:#0d0d0d;border:1px solid #222;padding:8px;overflow-x:auto}}a{{color:#fff}}summary{{cursor:pointer;color:#aaa}}\
         </style><h1>{t} API</h1><p>OpenAPI: <a href=/v1/openapi.json>/v1/openapi.json</a></p>{body}",
        t = esc(&st.title)
    );
    ([(header::CACHE_CONTROL, "no-cache")], Html(html)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use crate::{ErrorKind, Registry, Role};

    fn auth() -> Auth<Who> {
        Arc::new(|h: HeaderMap| {
            Box::pin(async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer admin") => Ok(Who(Role::Admin)),
                    Some("Bearer op") => Ok(Who(Role::Operator)),
                    Some("Bearer view") => Ok(Who(Role::Viewer)),
                    _ => Err(OpError::new(ErrorKind::Unauthorized, "who are you")),
                }
            })
        })
    }

    async fn serve(reg: Registry<Who>) -> String {
        let app = router(Arc::new(reg), auth(), "Test");
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    async fn req(m: reqwest::Method, url: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let c = reqwest::Client::new();
        let mut r = c.request(m, url).bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn rpc_rest_auth_and_errors() {
        let base = serve(registry()).await;
        let p = reqwest::Method::POST;
        assert_eq!(req(p.clone(), &format!("{base}/v1/ops/add"), "view", Some(json!({"a":1,"b":2}))).await, (200, json!(3)));
        assert_eq!(req(p.clone(), &format!("{base}/v1/ops/add"), "nobody", Some(json!({}))).await.0, 401);
        assert_eq!(req(p.clone(), &format!("{base}/v1/ops/nuke"), "op", None).await.0, 403);
        assert_eq!(req(p.clone(), &format!("{base}/v1/ops/nope"), "op", None).await.0, 404);
        let (s, v) = req(p.clone(), &format!("{base}/v1/ops/add"), "op", Some(json!({"a":1}))).await;
        assert_eq!((s, v["kind"].as_str()), (400, Some("bad_request")));

        // REST: path params and body merge; query params are coerced.
        let (s, v) = req(p.clone(), &format!("{base}/v1/things/t1/kick?times=3"), "op", Some(json!({"mode":"hard","tags":["a"]}))).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v, json!({"id":"t1","mode":"hard","tree":false,"tags":["a"],"times":3}));
        let (s, _) = req(p.clone(), &format!("{base}/v1/things/missing/kick"), "op", Some(json!({"mode":"soft"}))).await;
        assert_eq!(s, 404);
        let (s, v) = req(reqwest::Method::GET, &format!("{base}/v1/things"), "view", None).await;
        assert_eq!((s, v), (200, json!(["x", "y"])));
        // Wrong method on a REST path.
        assert_eq!(req(p, &format!("{base}/v1/things"), "view", None).await.0, 405);
    }

    #[tokio::test]
    async fn openapi_and_docs() {
        let base = serve(registry()).await;
        let doc: Value = reqwest::get(format!("{base}/v1/openapi.json")).await.unwrap().json().await.unwrap();
        assert_eq!(doc["openapi"], "3.1.0");
        let kick = &doc["paths"]["/v1/things/{id}/kick"]["post"];
        assert_eq!(kick["parameters"][0]["name"], "id");
        assert!(kick["requestBody"]["content"]["application/json"]["schema"]["properties"].get("id").is_none());
        // The enum lives in components and refs point there.
        assert!(doc["components"]["schemas"]["Mode"].is_object());
        let s = doc.to_string();
        assert!(!s.contains("#/$defs/"), "unrewritten ref");
        let list = &doc["paths"]["/v1/things"]["get"];
        assert_eq!(list["x-role"], "viewer");
        let html = reqwest::get(format!("{base}/v1/docs")).await.unwrap().text().await.unwrap();
        assert!(html.contains("kick_thing") && html.contains("POST /v1/things/{id}/kick"));
    }
}
