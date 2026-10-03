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
/// Deliveries a frozen hold keeps (the oldest go first).
const HOLD_MAX: usize = 1000;

/// A hold: while frozen, its routes' deliveries wait in it, in order.
#[derive(Default)]
struct Hold {
    frozen: bool,
    /// Unix ms.
    since: Option<u64>,
    queue: VecDeque<(i64, String, Payload)>,
    dropped: u64,
}

/// A change to a hold, for the database (written in order).
pub(crate) enum HoldWrite {
    Set { name: String, frozen: bool },
    Add { hold: String, seq: i64, route: String, payload: Value },
    Drop { hold: String, seq: i64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HoldSummary {
    pub name: String,
    pub frozen: bool,
    /// Frozen since (unix ms).
    pub since: Option<u64>,
    /// Deliveries waiting in it.
    pub queued: u32,
    /// Deliveries it dropped, full (since the hub started).
    pub dropped: u64,
    /// The routes going through it.
    pub routes: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Board {
    routes: Vec<Arc<Route>>,
    states: HashMap<String, RouteState>,
    /// Agents a route spawned that haven't finished their first turn.
    active: HashMap<String, HashSet<AgentId>>,
    /// Spawns in progress (slot reserved, agent not created yet).
    starting: HashMap<String, usize>,
    queued: HashMap<String, VecDeque<Action>>,
    holds: HashMap<String, Hold>,
    /// Per hold, one lane its routes deliver through, in the order dispatched
    /// (so what a release lets go comes before anything after it).
    lanes: HashMap<String, tokio::sync::mpsc::UnboundedSender<(Arc<Route>, Payload)>>,
    /// The last `seq` a held delivery got (they grow across restarts: unix ms × 1000).
    hold_seq: i64,
}

impl Board {
    /// Freezes or releases a hold; returns what a release lets go, in order.
    fn control(&mut self, name: &str, freeze: bool, writes: &mut Vec<HoldWrite>) -> Vec<(String, Payload)> {
        let h = self.holds.entry(name.to_string()).or_default();
        if h.frozen == freeze {
            return vec![];
        }
        h.frozen = freeze;
        h.since = freeze.then(now_ms);
        writes.push(HoldWrite::Set { name: name.to_string(), frozen: freeze });
        h.queue.drain(..).map(|(_, route, p)| (route, p)).collect()
    }

    /// Keeps a delivery in a frozen hold.
    fn keep(&mut self, hold: &str, route: &str, p: Payload, writes: &mut Vec<HoldWrite>) {
        self.hold_seq = (self.hold_seq + 1).max(now_ms() as i64 * 1000);
        let seq = self.hold_seq;
        let h = self.holds.entry(hold.to_string()).or_default();
        if h.queue.len() >= HOLD_MAX
            && let Some((old, ..)) = h.queue.pop_front()
        {
            h.dropped += 1;
            writes.push(HoldWrite::Drop { hold: hold.to_string(), seq: old });
        }
        writes.push(HoldWrite::Add { hold: hold.to_string(), seq, route: route.to_string(), payload: serde_json::to_value(&p).unwrap() });
        h.queue.push_back((seq, route.to_string(), p));
    }
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
        self.dispatch(ready);
    }

    /// Sends payloads on their way: first the holds they freeze or release
    /// (a release lets go of what the hold kept, before the rest), then each
    /// through its route's hold: kept while it's frozen, else delivered.
    fn dispatch(self: &Arc<Self>, ready: Vec<(Arc<Route>, Payload)>) {
        let mut writes = vec![];
        let mut free = vec![];
        {
            let mut b = self.board.lock().unwrap();
            for (r, _) in &ready {
                for d in &r.def.deliver {
                    if let Some(h) = &d.freeze {
                        b.control(h, true, &mut writes);
                    }
                    if let Some(h) = &d.release {
                        let let_go = b.control(h, false, &mut writes);
                        self.into_lane(&mut b, let_go);
                    }
                }
            }
            for (r, p) in ready {
                match r.def.hold.clone() {
                    Some(h) if b.holds.get(&h).is_some_and(|x| x.frozen) => b.keep(&h, &r.name, p, &mut writes),
                    Some(h) => self.lane(&mut b, &h, r, p),
                    None => free.push((r, p)),
                }
            }
        }
        for w in writes {
            let _ = self.hold_writes.send(w);
        }
        for (r, p) in free {
            self.clone().deliver(r, p);
        }
    }

    /// Delivers through a hold's lane (after everything before it there).
    fn lane(self: &Arc<Self>, b: &mut Board, hold: &str, r: Arc<Route>, p: Payload) {
        let tx = b.lanes.entry(hold.to_string()).or_insert_with(|| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Arc<Route>, Payload)>();
            let me = self.clone();
            tokio::spawn(async move {
                while let Some((r, p)) = rx.recv().await {
                    me.clone().deliver_now(r, p).await;
                }
            });
            tx
        });
        let _ = tx.send((r, p));
    }

    /// What a release let go, into its routes' lanes (routes since removed: dropped).
    fn into_lane(self: &Arc<Self>, b: &mut Board, let_go: Vec<(String, Payload)>) {
        for (name, p) in let_go {
            match b.routes.iter().find(|r| r.name == name).cloned() {
                Some(r) => {
                    let hold = r.def.hold.clone().unwrap_or_default();
                    self.lane(b, &hold, r, p);
                }
                None => tracing::warn!(route = %name, "a held delivery's route is gone; dropped"),
            }
        }
    }

    /// Freezes or releases a hold by hand (`freeze_hold`, `release_hold`).
    pub fn set_hold(self: &Arc<Self>, name: &str, freeze: bool) -> Result<HoldSummary, HubError> {
        let mut writes = vec![];
        {
            let mut b = self.board.lock().unwrap();
            if !b.routes.iter().any(|r| r.def.hold.as_deref() == Some(name)) {
                return Err(HubError::NotFound(format!("no route goes through hold {name:?}")));
            }
            let let_go = b.control(name, freeze, &mut writes);
            self.into_lane(&mut b, let_go);
        }
        for w in writes {
            let _ = self.hold_writes.send(w);
        }
        Ok(self.list_holds().into_iter().find(|h| h.name == name).unwrap())
    }

    /// Every hold routes go through.
    pub fn list_holds(&self) -> Vec<HoldSummary> {
        let b = self.board.lock().unwrap();
        let mut names: Vec<String> = b.routes.iter().filter_map(|r| r.def.hold.clone()).chain(b.holds.keys().cloned()).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let h = b.holds.get(&name);
                HoldSummary {
                    frozen: h.is_some_and(|h| h.frozen),
                    since: h.and_then(|h| h.since),
                    queued: h.map_or(0, |h| h.queue.len() as u32),
                    dropped: h.map_or(0, |h| h.dropped),
                    routes: b.routes.iter().filter(|r| r.def.hold.as_deref() == Some(&name)).map(|r| r.name.clone()).collect(),
                    name,
                }
            })
            .collect()
    }

    /// The holds as kept (on becoming leader).
    pub(crate) async fn load_holds(&self) -> Result<(), HubError> {
        let rows = self.db.holds().await?;
        let mut b = self.board.lock().unwrap();
        b.holds.clear();
        for row in rows {
            let mut queue = VecDeque::new();
            for (seq, route, payload) in row.held {
                match serde_json::from_value::<Payload>(payload) {
                    Ok(p) => queue.push_back((seq, route, p)),
                    Err(e) => tracing::warn!(hold = %row.name, seq, error = %e, "a held delivery can't be read; skipped"),
                }
                b.hold_seq = b.hold_seq.max(seq);
            }
            b.holds.insert(row.name, Hold { frozen: row.frozen, since: row.since.map(|s| s as u64), queue, dropped: 0 });
        }
        Ok(())
    }

    /// Writes holds' changes in the order they happened. Runs for the hub's life.
    pub(crate) async fn hold_writer(self: Arc<Self>, mut rx: tokio::sync::mpsc::UnboundedReceiver<HoldWrite>) {
        while let Some(w) = rx.recv().await {
            let r = match &w {
                HoldWrite::Set { name, frozen } => self.db.set_hold(name, *frozen).await,
                HoldWrite::Add { hold, seq, route, payload } => self.db.add_held(hold, *seq, route, payload).await,
                HoldWrite::Drop { hold, seq } => self.db.drop_held(hold, *seq).await,
            };
            if let Err(e) = r {
                tracing::error!(error = %e, "keeping a hold's change failed");
            }
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
                _ = self.stop.cancelled() => return,
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
            self.dispatch(ready);
        }
    }

    /// Performs a payload's actions in the background.
    fn deliver(self: Arc<Self>, r: Arc<Route>, p: Payload) {
        tokio::spawn(async move { self.deliver_now(r, p).await });
    }

    /// Performs a payload's actions and records the delivery.
    async fn deliver_now(self: Arc<Self>, r: Arc<Route>, p: Payload) {
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
            // Done when it was dispatched (before the event's other deliveries).
            Action::Freeze { .. } => Ok(json!("frozen")),
            Action::Release { .. } => Ok(json!("released")),
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
