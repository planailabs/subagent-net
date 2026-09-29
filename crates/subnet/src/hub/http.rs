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
        .layer(middleware::from_fn_with_state(hub.clone(), auth));
    Router::new()
        .route("/node", get(node_ws))
        .route("/streams", get(super::relay::streams_ws))
        .merge(events)
        .with_state(hub)
        .merge(subnet_ops::http::router(reg.clone(), auth_fn.clone(), "subagent-net"))
        .nest_service("/mcp", subnet_ops::mcp::service(reg, auth_fn, Some(instructions)))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

/// `Authorization: Bearer <token>`, or `?token=` for WebSocket clients that
/// can't set headers.
async fn auth(State(hub): State<Arc<Hub>>, Query(q): Query<TokenQuery>, req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    if hub.authenticate(bearer.or(q.token).as_deref()).is_ok() {
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
