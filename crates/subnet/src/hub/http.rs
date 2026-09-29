//! HTTP surface of the hub: the spawner WebSocket, the MCP endpoint for users
//! and clients, and the live event stream.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use subnet_core::addr::AgentId;
use subnet_core::proto::{ToHub, ToSpawner};
use tokio::sync::broadcast::error::RecvError;

use super::Hub;

const PING_EVERY: Duration = Duration::from_secs(10);
/// A spawner silent for this long is considered dead and its agents move.
const DEAD_AFTER: Duration = Duration::from_secs(30);

pub fn router(hub: Arc<Hub>) -> Router {
    // Spawners authenticate in their hello; users and clients per request.
    let reg = Arc::new(crate::api::registry(hub.clone()));
    let auth_fn = crate::api::auth(hub.clone());
    let instructions = "subagent-net hub: spawn agents, message them (answers arrive via wait_inbox), \
                        pause/resume/cancel/approve/fork them."
        .to_string();
    let events = Router::new().route("/events", get(events_ws)).layer(middleware::from_fn_with_state(hub.clone(), auth));
    Router::new()
        .route("/spawner", get(spawner_ws))
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
    if hub.token_ok(bearer.or(q.token).as_deref()) {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    agent: Option<AgentId>,
}

/// Streams committed events as JSON, optionally for one agent.
async fn events_ws(State(hub): State<Arc<Hub>>, Query(q): Query<EventsQuery>, ws: WebSocketUpgrade) -> Response {
    let mut rx = hub.subscribe();
    ws.on_upgrade(move |mut ws| async move {
        loop {
            let msg = match rx.recv().await {
                Ok(n) if q.agent.is_some_and(|a| a != n.agent) => continue,
                Ok(n) => serde_json::to_string(&n).unwrap(),
                Err(RecvError::Lagged(k)) => serde_json::json!({"lagged": k}).to_string(),
                Err(RecvError::Closed) => break,
            };
            if ws.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    })
}

async fn spawner_ws(State(hub): State<Arc<Hub>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve_spawner(hub, socket))
}

fn encode(m: &ToSpawner) -> Message {
    Message::Text(serde_json::to_string(m).unwrap().into())
}

async fn serve_spawner(hub: Arc<Hub>, mut ws: WebSocket) {
    let hello = match tokio::time::timeout(DEAD_AFTER, ws.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<ToHub>(&t),
        _ => return,
    };
    let hello = match hello {
        Ok(h) => h,
        Err(e) => {
            let _ = ws.send(encode(&ToSpawner::Rejected { reason: format!("bad hello: {e}") })).await;
            return;
        }
    };
    let (conn, mut rx) = match hub.connect(hello).await {
        Ok(x) => x,
        Err(reason) => {
            tracing::warn!(%reason, "spawner rejected");
            let _ = ws.send(encode(&ToSpawner::Rejected { reason })).await;
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
                            tracing::warn!(conn, error = %e, "spawner message failed");
                        },
                        Err(e) => tracing::warn!(conn, error = %e, "unparseable spawner message"),
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > DEAD_AFTER { tracing::warn!(conn, "spawner timed out"); break }
                if ws.send(Message::Ping(Default::default())).await.is_err() { break }
            }
        }
    }
    hub.disconnect(conn).await;
}
