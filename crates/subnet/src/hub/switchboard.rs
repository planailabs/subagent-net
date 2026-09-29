//! The switchboard at runtime: sense events → routes → deliveries.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::proto::Mail;
use subnet_switchboard::{Action, Counters, Payload, Route, RouteState, SenseEvent};

use super::{Hub, HubError, Notice};

/// Spawn actions waiting for a `max_active` slot, per route.
const QUEUE_MAX: usize = 1000;

#[derive(Default)]
pub(crate) struct Board {
    routes: Vec<Arc<Route>>,
    states: HashMap<String, RouteState>,
    /// Agents a route spawned that haven't finished their first turn.
    active: HashMap<String, HashSet<AgentId>>,
    /// Spawns in progress (slot reserved, agent not created yet).
    starting: HashMap<String, usize>,
    queued: HashMap<String, VecDeque<Action>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RouteSummary {
    pub name: String,
    pub from: String,
    pub counters: Counters,
    /// Spawned agents still on their first turn.
    pub active: u32,
    /// Spawns waiting for `max_active`.
    pub queued: u32,
    pub last_error: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

impl Hub {
    /// Installs a cluster's routes. State of routes whose definition didn't
    /// change is kept.
    pub(crate) fn set_routes(&self, routes: Vec<Route>) {
        let mut b = self.board.lock().unwrap();
        let old: HashMap<String, Arc<Route>> = b.routes.iter().map(|r| (r.name.clone(), r.clone())).collect();
        b.states.retain(|n, _| routes.iter().any(|r| &r.name == n && old.get(n).is_some_and(|o| o.def == r.def)));
        b.routes = routes.into_iter().map(Arc::new).collect();
        drop(b);
        self.board_wake.notify_one();
    }

    /// A sense event: notify subscribers and feed the routes listening to it.
    pub(crate) fn sense_event(self: &Arc<Self>, node: String, ev: SenseEvent) {
        let _ = self.notices.send(Notice::Sense {
            sense: ev.sense.clone(),
            node,
            id: ev.id.clone(),
            at: ev.at,
            data: ev.data.clone(),
        });
        let now = now_ms();
        let mut ready = vec![];
        {
            let mut b = self.board.lock().unwrap();
            let routes: Vec<Arc<Route>> = b.routes.iter().filter(|r| r.def.from == ev.sense).cloned().collect();
            for r in routes {
                let st = b.states.entry(r.name.clone()).or_default();
                ready.extend(st.offer(&r, &ev, now).into_iter().map(|p| (r.clone(), p)));
            }
        }
        self.board_wake.notify_one();
        for (r, p) in ready {
            self.clone().deliver(r, p);
        }
    }

    /// Fires debounce and batch windows as they come due. Runs for the hub's life.
    pub(crate) async fn board_timers(self: Arc<Self>) {
        loop {
            let next = {
                let b = self.board.lock().unwrap();
                b.states.values().filter_map(RouteState::next_due).min()
            };
            let wait = next.map(|t| Duration::from_millis(t.saturating_sub(now_ms())));
            tokio::select! {
                _ = self.board_wake.notified() => continue,
                _ = async { match wait { Some(w) => tokio::time::sleep(w).await, None => std::future::pending().await } } => {}
            }
            let now = now_ms();
            let mut ready = vec![];
            {
                let mut b = self.board.lock().unwrap();
                let routes = b.routes.clone();
                for r in routes {
                    if let Some(st) = b.states.get_mut(&r.name) {
                        ready.extend(st.tick(&r, now).into_iter().map(|p| (r.clone(), p)));
                    }
                }
            }
            for (r, p) in ready {
                self.clone().deliver(r, p);
            }
        }
    }

    /// Performs a payload's actions and records the delivery.
    fn deliver(self: Arc<Self>, r: Arc<Route>, p: Payload) {
        tokio::spawn(async move {
            let actions = match r.actions(&p) {
                Ok(a) => a,
                Err(e) => {
                    let mut b = self.board.lock().unwrap();
                    let st = b.states.entry(r.name.clone()).or_default();
                    st.counters.errors += 1;
                    st.last_error = Some(e);
                    return;
                }
            };
            let mut outcomes = vec![];
            for a in actions {
                let result = self.act(&r, a.clone()).await;
                outcomes.push(match result {
                    Ok(v) => json!({"action": a, "ok": v}),
                    Err(e) => json!({"action": a, "error": e}),
                });
            }
            let payload = serde_json::to_value(&p).unwrap();
            let outcomes = Value::Array(outcomes);
            if let Err(e) = self.db.add_delivery(&r.name, &payload, &outcomes).await {
                tracing::error!(route = %r.name, error = %e, "recording delivery failed");
            }
            let _ = self.notices.send(Notice::Delivery { route: r.name.clone(), payload, outcomes });
        });
    }

    async fn act(self: &Arc<Self>, r: &Route, a: Action) -> Result<Value, String> {
        let from = Addr::Route(r.name.clone());
        match a {
            Action::Spawn { .. } => {
                {
                    // Reserve a slot under the lock, so concurrent deliveries can't overshoot.
                    let mut b = self.board.lock().unwrap();
                    let used = b.active.get(&r.name).map_or(0, HashSet::len) + b.starting.get(&r.name).copied().unwrap_or(0);
                    if r.def.max_active.is_some_and(|max| used >= max as usize) {
                        let q = b.queued.entry(r.name.clone()).or_default();
                        if q.len() >= QUEUE_MAX {
                            q.pop_front();
                        }
                        q.push_back(a);
                        return Ok(json!("queued"));
                    }
                    *b.starting.entry(r.name.clone()).or_default() += 1;
                }
                self.route_spawn(&r.name, a).await
            }
            Action::Send { resident, content } => {
                self.send(&from, Addr::Resident(resident), content).await.map(|_| json!("sent")).map_err(|e| e.to_string())
            }
            Action::Mailbox { name, content } => {
                let mail = Mail { from, content, status: None };
                self.put_mail(&Addr::Mailbox(name), &mail).await.map(|_| json!("stored")).map_err(|e| e.to_string())
            }
            Action::Mcp { server, tool, args } => self.route_mcp(&server, &tool, args).await.map(Value::String),
        }
    }

    /// Spawns with a reserved slot (see `act`), which it turns into an active agent.
    async fn route_spawn(self: &Arc<Self>, route: &str, a: Action) -> Result<Value, String> {
        let Action::Spawn { mixture, prompt } = a else { unreachable!() };
        let r = self.spawn(&Addr::Route(route.to_string()), &mixture, prompt).await;
        let mut b = self.board.lock().unwrap();
        if let Some(n) = b.starting.get_mut(route) {
            *n = n.saturating_sub(1);
        }
        let s = r.map_err(|e| e.to_string())?;
        b.active.entry(route.to_string()).or_default().insert(s.id);
        Ok(json!({"spawned": s.id}))
    }

    /// An agent a route spawned answered (or failed): free its slot and start
    /// the next queued spawn.
    pub(crate) fn route_agent_done(self: &Arc<Self>, route: &str, agent: AgentId) {
        let next = {
            let mut b = self.board.lock().unwrap();
            let was = b.active.get_mut(route).is_some_and(|s| s.remove(&agent));
            if !was {
                return;
            }
            let next = b.queued.get_mut(route).and_then(VecDeque::pop_front);
            if next.is_some() {
                *b.starting.entry(route.to_string()).or_default() += 1;
            }
            next
        };
        if let Some(a) = next {
            let (me, route) = (self.clone(), route.to_string());
            tokio::spawn(async move {
                if let Err(e) = me.route_spawn(&route, a).await {
                    tracing::warn!(%route, error = %e, "queued spawn failed");
                }
            });
        }
    }

    /// A route calling an MCP tool directly, on any node running the server.
    /// Idempotent tools are retried (3 attempts).
    async fn route_mcp(&self, server: &str, tool: &str, args: Value) -> Result<String, String> {
        let (id, idempotent) = {
            let c = self.cluster.read().unwrap();
            let id = c.spec.mcp_id(server).ok_or_else(|| format!("no mcp {server:?}"))?;
            (id, c.spec.mcps[server].idempotent.iter().any(|t| t == tool))
        };
        let attempts = if idempotent { 3 } else { 1 };
        let mut last = String::new();
        for i in 0..attempts {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(250 << i)).await;
            }
            match self.mcp_from_hub(&id, tool, args.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub async fn list_routes(&self) -> Vec<RouteSummary> {
        let b = self.board.lock().unwrap();
        b.routes
            .iter()
            .map(|r| {
                let st = b.states.get(&r.name);
                RouteSummary {
                    name: r.name.clone(),
                    from: r.def.from.clone(),
                    counters: st.map(|s| s.counters.clone()).unwrap_or_default(),
                    active: b.active.get(&r.name).map_or(0, |s| s.len() as u32),
                    queued: b.queued.get(&r.name).map_or(0, |q| q.len() as u32),
                    last_error: st.and_then(|s| s.last_error.clone()),
                }
            })
            .collect()
    }

    pub async fn list_deliveries(&self, route: Option<&str>, limit: u32) -> Result<Vec<super::db::DeliveryRow>, HubError> {
        Ok(self.db.deliveries(route, limit.clamp(1, 1000)).await?)
    }

    pub async fn list_senses(&self) -> Vec<crate::api::SenseSummary> {
        let c = self.cluster.read().unwrap().spec.clone();
        let st = self.st.lock().await;
        c.senses
            .iter()
            .map(|(name, s)| {
                let status = st.nodes.values().find(|n| n.name == s.node).and_then(|n| n.senses.get(name).cloned());
                let source = match s.source.kind() {
                    Ok(subnet_cluster::SourceKind::Exec(_)) => "exec",
                    Ok(subnet_cluster::SourceKind::ExecStream(..)) => "stream-publisher",
                    Ok(subnet_cluster::SourceKind::Subscribe(_)) => "stream-subscriber",
                    Ok(subnet_cluster::SourceKind::Webhook(_)) => "webhook",
                    Ok(subnet_cluster::SourceKind::Timer(_)) => "timer",
                    Ok(subnet_cluster::SourceKind::File(_)) => "file",
                    Err(_) => "invalid",
                };
                crate::api::SenseSummary {
                    name: name.clone(),
                    node: s.node.clone(),
                    source: source.into(),
                    stages: s.stage.keys().cloned().collect(),
                    running: matches!(status, Some(None)),
                    error: status.flatten(),
                }
            })
            .collect()
    }

    pub async fn peek_mail(&self, addr: &Addr, max: u32) -> Result<Vec<Mail>, HubError> {
        Ok(self.db.peek_mail(addr, max.clamp(1, 1000)).await?)
    }

    /// Feeds an event as if a sense had produced it (tests, the web UI).
    pub fn inject_event(self: &Arc<Self>, sense: &str, data: Value) -> Result<String, HubError> {
        if !self.cluster.read().unwrap().spec.senses.contains_key(sense) {
            return Err(HubError::NotFound(format!("no sense {sense:?}")));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let ev = SenseEvent { id: id.clone(), sense: sense.into(), at: now_ms(), data };
        self.sense_event("(injected)".into(), ev);
        Ok(id)
    }
}
