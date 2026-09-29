//! HTTP surface of the hub: the spawner WebSocket.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use subnet_core::proto::{ToHub, ToSpawner};

use super::Hub;

const PING_EVERY: Duration = Duration::from_secs(10);
/// A spawner silent for this long is considered dead and its agents move.
const DEAD_AFTER: Duration = Duration::from_secs(30);

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new().route("/spawner", get(spawner_ws)).with_state(hub)
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
