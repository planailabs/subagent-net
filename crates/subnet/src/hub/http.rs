//! HTTP surface of the hub: the node WebSocket, the MCP endpoint for users
//! and clients, and the live event stream.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use crate::wire::{ToHub, ToNode};
use futures::StreamExt;
use tokio::sync::broadcast::error::RecvError;

use super::{Hub, NoticeFilter};

const PING_EVERY: Duration = Duration::from_secs(10);
/// A node silent for this long is considered dead and its agents move.
const DEAD_AFTER: Duration = Duration::from_secs(30);

pub fn router(hub: Arc<Hub>) -> Router {
    // Nodes authenticate in their hello; users and clients per request.
    let reg = Arc::new(crate::api::registry(hub.clone()));
    let auth_fn = crate::api::auth(hub.clone());
    let instructions = "subagent-net hub: spawn agents, message them (answers arrive via wait_inbox), \
                        pause/resume/cancel/approve/fork them."
        .to_string();
    let events = Router::new()
        .route("/v1/events", get(events_sse))
        .route("/v1/events/ws", get(events_ws))
        .route("/v1/blobs/{hash}/raw", get(blob_raw))
        .layer(middleware::from_fn_with_state(hub.clone(), auth));
    let guard = middleware::from_fn_with_state(hub.clone(), leader_guard);
    Router::new()
        .route("/node", get(node_ws))
        .route("/v1/login", axum::routing::post(login))
        .route("/v1/logout", axum::routing::post(logout))
        .route("/streams", get(super::relay::streams_ws))
        .merge(events)
        .with_state(hub)
        .merge(subnet_ops::http::router(reg.clone(), auth_fn.clone(), "subagent-net"))
        .nest_service("/mcp", subnet_ops::mcp::service(reg, auth_fn, Some(instructions)))
        .fallback(get(web_ui))
        .layer(guard)
}

#[derive(Deserialize)]
struct Login {
    token: String,
}

/// Checks a token and keeps it in an HttpOnly session cookie (web UI).
async fn login(State(hub): State<Arc<Hub>>, axum::Json(l): axum::Json<Login>) -> Response {
    match hub.authenticate(Some(&l.token)) {
        Ok(p) => {
            let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", crate::api::SESSION_COOKIE, l.token);
            let who = serde_json::json!({"addr": p.addr, "role": p.role});
            ([(axum::http::header::SET_COOKIE, cookie)], axum::Json(who)).into_response()
        }
        Err(e) => subnet_ops::OpError::from(e).into_response(),
    }
}

async fn logout() -> Response {
    let cookie = format!("{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0", crate::api::SESSION_COOKIE);
    ([(axum::http::header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response()
}

#[cfg(feature = "webui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../webui/dist"]
struct WebUi;

/// The web UI; unknown paths get index.html (the app routes by hash).
#[cfg(feature = "webui")]
async fn web_ui(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (path, file) = match WebUi::get(path) {
        Some(f) if !path.is_empty() => (path, f),
        _ => match WebUi::get("index.html") {
            Some(f) => ("index.html", f),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path == "index.html" { "no-cache" } else { "public, max-age=31536000, immutable" };
    ([(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string()), (axum::http::header::CACHE_CONTROL, cache.into())], file.data)
        .into_response()
}

#[cfg(not(feature = "webui"))]
async fn web_ui() -> Response {
    (StatusCode::NOT_FOUND, "this hub was built without the web UI").into_response()
}

/// Standbys serve nothing: they answer 503 and name the leader.
async fn leader_guard(State(hub): State<Arc<Hub>>, req: Request, next: Next) -> Response {
    if hub.is_leader() {
        return next.run(req).await;
    }
    let mut r = (StatusCode::SERVICE_UNAVAILABLE, "not the leader").into_response();
    if let Some(url) = hub.leader_url()
        && let Ok(v) = axum::http::HeaderValue::from_str(&url)
    {
        r.headers_mut().insert(subnet_ops::client::LEADER_HEADER, v);
    }
    r
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// `Authorization: Bearer <token>`, or `?token=` for WebSocket clients that
/// can't set headers.
async fn auth(State(hub): State<Arc<Hub>>, Query(q): Query<TokenQuery>, req: Request, next: Next) -> Response {
    let token = crate::api::token_from(req.headers()).or(q.token);
    if hub.authenticate(token.as_deref()).is_ok() {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn notices(hub: &Hub, f: NoticeFilter) -> impl futures::Stream<Item = String> + use<> {
    futures::stream::unfold(hub.subscribe(), move |mut rx| {
        let f = f.clone();
        async move {
            loop {
                match rx.recv().await {
                    Ok(n) if !f.matches(&n) => continue,
                    Ok(n) => return Some((serde_json::to_string(&n).unwrap(), rx)),
                    Err(RecvError::Lagged(k)) => return Some((serde_json::json!({"kind":"lagged","missed":k}).to_string(), rx)),
                    Err(RecvError::Closed) => return None,
                }
            }
        }
    })
}

/// A blob's bytes with its content type.
async fn blob_raw(State(hub): State<Arc<Hub>>, axum::extract::Path(hash): axum::extract::Path<String>) -> Response {
    match hub.get_blob(&hash).await {
        Ok((mime, data)) => ([(axum::http::header::CONTENT_TYPE, mime)], data).into_response(),
        Err(e) => subnet_ops::OpError::from(e).into_response(),
    }
}

/// Server-sent events.
async fn events_sse(State(hub): State<Arc<Hub>>, Query(f): Query<NoticeFilter>) -> Response {
    let s = notices(&hub, f).map(|j| Ok::<_, std::convert::Infallible>(SseEvent::default().data(j)));
    Sse::new(s).keep_alive(KeepAlive::default()).into_response()
}

async fn events_ws(State(hub): State<Arc<Hub>>, Query(f): Query<NoticeFilter>, ws: WebSocketUpgrade) -> Response {
    let s = notices(&hub, f);
    ws.on_upgrade(move |mut ws| async move {
        futures::pin_mut!(s);
        loop {
            tokio::select! {
                n = s.next() => {
                    let Some(n) = n else { break };
                    if ws.send(Message::Text(n.into())).await.is_err() { break }
                }
                inc = ws.recv() => if !matches!(inc, Some(Ok(_))) { break },
            }
        }
    })
}

async fn node_ws(State(hub): State<Arc<Hub>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve_node(hub, socket))
}

fn encode(m: &ToNode) -> Message {
    Message::Text(serde_json::to_string(m).unwrap().into())
}

async fn serve_node(hub: Arc<Hub>, mut ws: WebSocket) {
    let hello = match tokio::time::timeout(DEAD_AFTER, ws.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<ToHub>(&t),
        _ => return,
    };
    let hello = match hello {
        Ok(h) => h,
        Err(e) => {
            let _ = ws.send(encode(&ToNode::Rejected { reason: format!("bad hello: {e}") })).await;
            return;
        }
    };
    let (conn, mut rx) = match hub.connect(hello).await {
        Ok(x) => x,
        Err(reason) => {
            tracing::warn!(%reason, "node rejected");
            let _ = ws.send(encode(&ToNode::Rejected { reason })).await;
            return;
        }
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut last_seen = tokio::time::Instant::now();
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(m) = out else { break };
                if ws.send(encode(&m)).await.is_err() { break }
            }
            inc = ws.recv() => {
                last_seen = tokio::time::Instant::now();
                match inc {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ToHub>(&t) {
                        Ok(m) => if let Err(e) = hub.handle(conn, m).await {
                            tracing::warn!(conn, error = %e, "node message failed");
                        },
                        Err(e) => tracing::warn!(conn, error = %e, "unparseable node message"),
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > DEAD_AFTER { tracing::warn!(conn, "node timed out"); break }
                if ws.send(Message::Ping(Default::default())).await.is_err() { break }
            }
        }
    }
    hub.disconnect(conn).await;
}
