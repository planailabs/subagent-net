//! The "world" MCP server: how Vesper acts. Streamable HTTP at `/mcp`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::Timelike;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use crate::server::Room;
use crate::world::{SPEED, Target};

#[derive(Clone)]
pub struct WorldTools(pub Arc<Room>);

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Say {
    /// What to say, as spoken words.
    pub text: String,
    /// The person you're talking to, if anyone (you turn to them).
    pub to: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Where {
    /// A thing's id (e.g. "coffee_maker"), a person's name, or a point {"x": .., "z": ..} in metres.
    pub target: Value,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Gesture {
    /// wave, nod, shrug or think.
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Interact {
    /// The thing's id, from look_around.
    pub object: String,
    /// One of the thing's actions, from look_around.
    pub action: String,
}

#[tool_router(server_handler)]
impl WorldTools {
    #[tool(description = "Say something out loud; everyone in the room hears your voice. Returns when you've finished speaking.")]
    async fn say(&self, Parameters(Say { text, to }): Parameters<Say>) -> Result<String, String> {
        let room = &self.0;
        if let Some(to) = &to {
            let people = room.people();
            // Talking to someone who isn't here is fine; she just doesn't turn.
            let _ = room.world.lock().unwrap().look_at(&Target::Thing(to.clone()), &people);
        }
        let secs = room.say(&text, to.as_deref()).await.map_err(|e| format!("couldn't speak: {e}"))?;
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        Ok(format!("said it ({secs:.1} s)"))
    }

    #[tool(description = "Walk to a thing, a person or a point. Returns when you've arrived.")]
    async fn move_to(&self, Parameters(Where { target }): Parameters<Where>) -> Result<String, String> {
        let room = &self.0;
        let target = Target::parse(&target)?;
        // A person is at the viewers' side of the room.
        let target = match &target {
            Target::Thing(id) if room.world.lock().unwrap().object(id).is_none() && room.people().iter().any(|p| p.eq_ignore_ascii_case(id)) => {
                Target::Point([crate::world::VIEWERS[0], 2.4])
            }
            _ => target,
        };
        let (len, walk) = {
            let mut w = room.world.lock().unwrap();
            let len = w.move_to(&target)?;
            (len, room.walks.fetch_add(1, Ordering::SeqCst) + 1)
        };
        let what = match &target {
            Target::Thing(id) => format!("the {id}"),
            Target::Point([x, z]) => format!("({x:.1}, {z:.1})"),
        };
        if len == 0.0 {
            return Ok(format!("you're already at {what}"));
        }
        let deadline = Instant::now() + Duration::from_secs_f64(len as f64 / SPEED as f64 / room.cfg.time_scale + 5.0);
        loop {
            room.tick().await;
            if room.walks.load(Ordering::SeqCst) != walk {
                return Ok(format!("stopped on the way to {what}: you're walking somewhere else now"));
            }
            if !room.world.lock().unwrap().walking() {
                return Ok(format!("arrived at {what} ({len:.1} m)"));
            }
            if Instant::now() > deadline {
                return Err(format!("didn't reach {what}"));
            }
        }
    }

    #[tool(description = "Turn to face a thing, a person or a point.")]
    fn look_at(&self, Parameters(Where { target }): Parameters<Where>) -> Result<String, String> {
        let target = Target::parse(&target)?;
        let people = self.0.people();
        self.0.world.lock().unwrap().look_at(&target, &people)?;
        Ok("turned".into())
    }

    #[tool(description = "Make a gesture: wave, nod, shrug or think.")]
    fn gesture(&self, Parameters(Gesture { name }): Parameters<Gesture>) -> Result<String, String> {
        let secs = self.0.world.lock().unwrap().gesture(&name)?;
        Ok(format!("{name} ({secs:.1} s)"))
    }

    #[tool(description = "Use a thing in the room. You must be next to it (move_to it first). look_around lists every thing's actions.")]
    fn interact(&self, Parameters(Interact { object, action }): Parameters<Interact>) -> Result<String, String> {
        let hour = chrono::Local::now().hour();
        self.0.world.lock().unwrap().interact(&object, &action, hour)
    }

    #[tool(description = "Look around: where you are, who's in the room, and every thing with its state, distance and actions.")]
    fn look_around(&self) -> String {
        let people = self.0.people();
        self.0.world.lock().unwrap().look_around(&people).to_string()
    }
}

/// `/mcp`, behind a bearer token if one is configured.
pub fn router(room: Arc<Room>) -> Router {
    let token = room.cfg.mcp_token.clone();
    let svc = StreamableHttpService::new(move || Ok(WorldTools(room.clone())), LocalSessionManager::default().into(), StreamableHttpServerConfig::default());
    Router::new().nest_service("/mcp", svc).layer(axum::middleware::from_fn(move |req: Request, next: Next| {
        let token = token.clone();
        async move {
            let ok = match &token {
                None => true,
                Some(t) => req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {t}")),
            };
            if ok { next.run(req).await } else { StatusCode::UNAUTHORIZED.into_response() as Response }
        }
    }))
}
