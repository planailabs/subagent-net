//! The hub: sequencer of every agent's event log, type registry, placement,
//! fencing and message routing. Transports (WebSocket, MCP) are thin layers
//! over `Hub::connect`, `Hub::handle` and `Hub::op`.

pub mod db;
pub mod http;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, broadcast, mpsc};
use uuid::Uuid;

use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Agent, Budget, Effect, Event, Phase, Spec};
use subnet_core::proto::{Mail, Op, ToHub, ToSpawner, TypeInfo};

use db::Db;

pub type ConnId = u64;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    Bad(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

fn bad<T>(s: impl Into<String>) -> Result<T, HubError> {
    Err(HubError::Bad(s.into()))
}

/// A committed event, as broadcast to subscribers.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    pub agent: AgentId,
    pub seq: u64,
    pub event: Event,
}

struct AgentRec {
    a: Agent,
    seq: u64,
    epoch: u64,
    spawner: Option<ConnId>,
}

struct SpawnerRec {
    name: String,
    types: Vec<TypeInfo>,
    capacity: u32,
    tx: mpsc::UnboundedSender<ToSpawner>,
    agents: HashSet<AgentId>,
}

#[derive(Default)]
struct State {
    agents: HashMap<AgentId, AgentRec>,
    spawners: HashMap<ConnId, SpawnerRec>,
    next_conn: ConnId,
}

pub struct Hub {
    db: Db,
    // ponytail: one global lock serialises all commits; shard per agent if it becomes the bottleneck.
    st: Mutex<State>,
    token: Option<String>,
    mail: Notify,
    notices: broadcast::Sender<Notice>,
}

/// Events only a spawner may propose; everything else originates at the hub.
fn spawner_event(e: &Event) -> bool {
    matches!(
        e,
        Event::LlmDelta { .. }
            | Event::LlmDone
            | Event::LlmAborted
            | Event::LlmFailed { .. }
            | Event::ToolResult { .. }
            | Event::ToolAborted { .. }
    )
}

impl Hub {
    pub async fn open(db_url: &str, token: Option<String>) -> Result<Arc<Self>, HubError> {
        let db = Db::connect(db_url).await?;
        let mut st = State::default();
        for row in db.agents().await? {
            let events = db.events(row.id, 0).await?;
            let a = Agent::replay(row.id, row.spec, &events);
            st.agents.insert(row.id, AgentRec { a, seq: events.len() as u64, epoch: row.epoch, spawner: None });
        }
        tracing::info!(agents = st.agents.len(), "hub loaded");
        Ok(Arc::new(Self { db, st: Mutex::new(st), token, mail: Notify::new(), notices: broadcast::channel(4096).0 }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }

    // ---------- spawner connections ----------

    pub async fn connect(&self, hello: ToHub) -> Result<(ConnId, mpsc::UnboundedReceiver<ToSpawner>), String> {
        let ToHub::Hello { name, token, types, capacity } = hello else {
            return Err("expected hello".into());
        };
        if self.token.is_some() && token != self.token {
            return Err("bad token".into());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let mut st = self.st.lock().await;
        let conn = st.next_conn;
        st.next_conn += 1;
        tracing::info!(conn, %name, types = ?types.iter().map(TypeInfo::id).collect::<Vec<_>>(), capacity, "spawner connected");
        let _ = tx.send(ToSpawner::Welcome);
        st.spawners.insert(conn, SpawnerRec { name, types, capacity, tx, agents: HashSet::new() });
        if let Err(e) = self.place_pending(&mut st).await {
            tracing::error!(error = %e, "placement after connect failed");
        }
        Ok((conn, rx))
    }

    pub async fn disconnect(&self, conn: ConnId) {
        let mut st = self.st.lock().await;
        let Some(s) = st.spawners.remove(&conn) else { return };
        tracing::warn!(conn, name = %s.name, agents = s.agents.len(), "spawner gone, reassigning its agents");
        for id in s.agents {
            if let Some(r) = st.agents.get_mut(&id) {
                r.spawner = None;
            }
        }
        if let Err(e) = self.place_pending(&mut st).await {
            tracing::error!(error = %e, "placement after disconnect failed");
        }
    }

    pub async fn handle(&self, conn: ConnId, msg: ToHub) -> Result<(), HubError> {
        match msg {
            ToHub::Hello { .. } => bad("duplicate hello"),
            ToHub::Propose { agent, epoch, events } => {
                let mut st = self.st.lock().await;
                if !self.owns(&st, conn, agent, epoch) {
                    return Ok(());
                }
                if let Some(e) = events.iter().find(|e| !spawner_event(e)) {
                    return bad(format!("spawner may not propose {e:?}"));
                }
                self.commit(&mut st, agent, events).await
            }
            ToHub::Request { id, agent, epoch, op } => {
                let owns = self.owns(&*self.st.lock().await, conn, agent, epoch);
                let result = if owns {
                    self.op(&Addr::Agent(agent), op).await
                } else {
                    Err("stale epoch".into())
                };
                self.send(conn, ToSpawner::Reply { id, result }).await;
                Ok(())
            }
        }
    }

    /// Fencing: only the current owner at the current epoch may write. A stale
    /// writer is told to stop.
    fn owns(&self, st: &State, conn: ConnId, agent: AgentId, epoch: u64) -> bool {
        let ok = st.agents.get(&agent).is_some_and(|r| r.epoch == epoch && r.spawner == Some(conn));
        if !ok {
            tracing::warn!(conn, %agent, epoch, "rejected write from non-owner");
            if let Some(s) = st.spawners.get(&conn) {
                let _ = s.tx.send(ToSpawner::Revoke { agent });
            }
        }
        ok
    }

    async fn send(&self, conn: ConnId, msg: ToSpawner) {
        if let Some(s) = self.st.lock().await.spawners.get(&conn) {
            let _ = s.tx.send(msg);
        }
    }

    // ---------- log ----------

    /// Appends events to an agent's log, updates the replica, forwards them to
    /// the owning spawner and routes resulting reports. Reports are routed here,
    /// from the hub's replica, so each is delivered exactly once whatever
    /// happens to the spawner.
    async fn commit(&self, st: &mut State, id: AgentId, events: Vec<Event>) -> Result<(), HubError> {
        let mut work = std::collections::VecDeque::from([(id, events)]);
        while let Some((id, events)) = work.pop_front() {
            if events.is_empty() {
                continue;
            }
            let Some(r) = st.agents.get_mut(&id) else { return bad(format!("no agent {id}")) };
            self.db.append(id, r.seq + 1, &events).await?;
            let mut reports = vec![];
            for event in events {
                r.seq += 1;
                for fx in r.a.apply(&event) {
                    if let Effect::Report { to, status, content } = fx {
                        reports.push((to, status, content));
                    }
                }
                let seq = r.seq;
                if let Some(s) = r.spawner.and_then(|c| st.spawners.get(&c)) {
                    let _ = s.tx.send(ToSpawner::Commit { agent: id, seq, event: event.clone() });
                }
                let _ = self.notices.send(Notice { agent: id, seq, event });
            }
            if r.a.phase == Phase::Cancelled
                && let Some(c) = r.spawner.take()
                && let Some(s) = st.spawners.get_mut(&c)
            {
                s.agents.remove(&id);
                let _ = s.tx.send(ToSpawner::Revoke { agent: id });
            }
            let parent = r.a.spec.parent;
            for (to, status, content) in reports {
                for addr in to {
                    match addr {
                        Addr::Agent(p) if Some(p) == parent => {
                            work.push_back((p, vec![Event::ChildReport { id, status, content: content.clone() }]));
                        }
                        Addr::Agent(x) if st.agents.contains_key(&x) => {
                            work.push_back((x, vec![Event::Inbox { from: Addr::Agent(id), content: content.clone() }]));
                        }
                        Addr::Agent(x) => tracing::warn!(from = %id, to = %x, "report to unknown agent dropped"),
                        other => {
                            let mail = Mail { from: Addr::Agent(id), content: content.clone(), status: Some(status) };
                            self.put_mail(&other, &mail).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ---------- placement ----------

    async fn place_pending(&self, st: &mut State) -> Result<(), HubError> {
        let pending: Vec<_> = st
            .agents
            .iter()
            .filter(|(_, r)| r.spawner.is_none() && r.a.phase != Phase::Cancelled)
            .map(|(id, _)| *id)
            .collect();
        for id in pending {
            self.place(st, id).await?;
        }
        Ok(())
    }

    /// Assigns an agent to the least-loaded live spawner offering its exact
    /// type. Without one it stays pending until such a spawner connects.
    async fn place(&self, st: &mut State, id: AgentId) -> Result<(), HubError> {
        let r = &st.agents[&id];
        let ty = r.a.spec.ty.clone();
        let Some((&conn, _)) = st
            .spawners
            .iter()
            .filter(|(_, s)| s.types.iter().any(|t| t.id() == ty) && (s.agents.len() as u32) < s.capacity)
            .min_by_key(|(c, s)| (s.agents.len(), **c))
        else {
            tracing::debug!(%id, %ty, "no spawner for agent, pending");
            return Ok(());
        };
        let epoch = r.epoch + 1;
        self.db.set_epoch(id, epoch).await?;
        let events = self.db.events(id, 0).await?;
        let r = st.agents.get_mut(&id).unwrap();
        r.epoch = epoch;
        r.spawner = Some(conn);
        let s = st.spawners.get_mut(&conn).unwrap();
        s.agents.insert(id);
        tracing::info!(%id, spawner = %s.name, epoch, "assigned");
        let _ = s.tx.send(ToSpawner::Assign { agent: id, epoch, spec: r.a.spec.clone(), events });
        Ok(())
    }

    async fn put_mail(&self, to: &Addr, mail: &Mail) -> Result<(), HubError> {
        self.db.put_mail(to, mail).await?;
        self.mail.notify_waiters();
        Ok(())
    }

    // ---------- ops ----------

    pub async fn op(&self, caller: &Addr, op: Op) -> Result<Value, String> {
        self.op_inner(caller, op).await.map_err(|e| e.to_string())
    }

    async fn op_inner(&self, caller: &Addr, op: Op) -> Result<Value, HubError> {
        if let Op::WaitInbox { timeout_ms } = op {
            return self.wait_inbox(caller, timeout_ms).await;
        }
        let mut st = self.st.lock().await;
        let st = &mut *st;
        match op {
            Op::Spawn { ty, prompt } => self.spawn(st, caller, &ty, prompt).await,
            Op::Send { to, content } => {
                match &to {
                    Addr::Agent(id) if st.agents.contains_key(id) => {
                        self.commit(st, *id, vec![Event::Inbox { from: caller.clone(), content }]).await?
                    }
                    Addr::Agent(id) => return bad(format!("no agent {id}")),
                    other => self.put_mail(other, &Mail { from: caller.clone(), content, status: None }).await?,
                }
                Ok(json!({"sent": true}))
            }
            Op::ListAgents => Ok(Value::Array(st.agents.iter().map(|(id, r)| summary(st, *id, r)).collect())),
            Op::ListTypes => Ok(list_types(st)),
            Op::Pause { id, mode, tree } => {
                let ids = self.targets(st, caller, id, tree)?;
                for id in ids {
                    self.commit(st, id, vec![Event::PauseRequested { mode }]).await?;
                }
                Ok(json!({"paused": true}))
            }
            Op::Resume { id, tree } => {
                let ids = self.targets(st, caller, id, tree)?;
                for id in ids {
                    self.commit(st, id, vec![Event::Resumed]).await?;
                }
                Ok(json!({"resumed": true}))
            }
            Op::Cancel { id } => {
                let ids = self.targets(st, caller, id, true)?;
                for id in ids {
                    self.commit(st, id, vec![Event::Cancelled]).await?;
                }
                Ok(json!({"cancelled": true}))
            }
            Op::Approve { id, call_id, approved } => {
                self.targets(st, caller, id, false)?;
                self.commit(st, id, vec![Event::Approval { call_id, approved }]).await?;
                Ok(json!({"ok": true}))
            }
            Op::Fork { id, at } => {
                if matches!(caller, Addr::Agent(_)) {
                    return bad("agents may not fork");
                }
                let Some(r) = st.agents.get(&id) else { return bad(format!("no agent {id}")) };
                let mut events = self.db.events(id, 0).await?;
                events.truncate(at.unwrap_or(r.seq) as usize);
                let spec = Spec { parent: None, ..r.a.spec.clone() };
                let new = Uuid::new_v4();
                self.db.create_agent(new, &spec).await?;
                let a = Agent::new(new, spec);
                st.agents.insert(new, AgentRec { a, seq: 0, epoch: 0, spawner: None });
                self.commit(st, new, events).await?;
                self.place(st, new).await?;
                Ok(json!({"id": new}))
            }
            Op::Transcript { id } => {
                let Some(r) = st.agents.get(&id) else { return bad(format!("no agent {id}")) };
                let mut v = summary(st, id, r);
                v["messages"] = json!(r.a.messages);
                v["partial"] = if r.a.acc.is_empty() { Value::Null } else { json!(r.a.acc.partial()) };
                v["inbox"] = json!(r.a.inbox);
                Ok(v)
            }
            Op::WaitInbox { .. } => unreachable!(),
        }
    }

    async fn wait_inbox(&self, caller: &Addr, timeout_ms: Option<u64>) -> Result<Value, HubError> {
        if matches!(caller, Addr::Agent(_)) {
            return bad("agents receive messages in their inbox");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.unwrap_or(30_000).min(300_000));
        loop {
            let notified = self.mail.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mail = self.db.take_mail(caller).await?;
            if !mail.is_empty() {
                return Ok(json!(mail));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(json!([]));
            }
        }
    }

    /// Resolves targets of a control op and checks the caller may act on them:
    /// users and clients may act on anyone, agents only on their descendants.
    fn targets(&self, st: &State, caller: &Addr, id: AgentId, tree: bool) -> Result<Vec<AgentId>, HubError> {
        if !st.agents.contains_key(&id) {
            return bad(format!("no agent {id}"));
        }
        if let Addr::Agent(me) = caller
            && !is_ancestor(st, *me, id)
        {
            return bad(format!("{id} is not a descendant of {caller}"));
        }
        let mut out = vec![id];
        if tree {
            let mut i = 0;
            while i < out.len() {
                let p = out[i];
                out.extend(st.agents.iter().filter(|(_, r)| r.a.spec.parent == Some(p)).map(|(c, _)| *c));
                i += 1;
            }
        }
        Ok(out)
    }

    async fn spawn(&self, st: &mut State, caller: &Addr, ty: &str, prompt: String) -> Result<Value, HubError> {
        // Newest registration wins when several hashes share a name.
        let Some(info) = st.spawners.values().flat_map(|s| &s.types).filter(|t| t.name == ty || t.id() == ty).last().cloned()
        else {
            return bad(format!("no live spawner offers type {ty:?}"));
        };
        let (parent, budget, reserved) = match caller {
            Addr::Agent(p) => {
                let r = &st.agents[p];
                let parent_info = st.spawners.values().flat_map(|s| &s.types).find(|t| t.id() == r.a.spec.ty);
                if !parent_info.is_some_and(|t| t.spawns.contains(&info.name)) {
                    return bad(format!("type {} may not spawn {}", r.a.spec.ty, info.name));
                }
                let pb = &r.a.spec.budget;
                if pb.max_depth == 0 {
                    return bad("depth budget exhausted");
                }
                if r.a.children.len() as u32 >= pb.max_children {
                    return bad(format!("child budget exhausted ({} children)", pb.max_children));
                }
                // An unlimited type under a limited parent gets half of what is left,
                // so the parent is never starved by its first child.
                let max_tokens = match (info.budget.max_tokens, r.a.remaining_tokens()) {
                    (Some(t), Some(left)) => Some(t.min(left)),
                    (t, None) => t,
                    (None, Some(left)) => Some(left / 2),
                };
                if max_tokens == Some(0) {
                    return bad("token budget exhausted");
                }
                let budget = Budget {
                    max_tokens,
                    max_depth: info.budget.max_depth.min(pb.max_depth - 1),
                    max_children: info.budget.max_children,
                };
                // Only a limited parent reserves; an unlimited one has nothing to carve from.
                let reserved = r.a.spec.budget.max_tokens.and(max_tokens).unwrap_or(0);
                (Some(*p), budget, reserved)
            }
            _ => (None, info.budget.clone(), 0),
        };
        let spec = Spec { ty: info.id(), parent, budget, approve: info.approve.clone() };
        let id = Uuid::new_v4();
        self.db.create_agent(id, &spec).await?;
        st.agents.insert(id, AgentRec { a: Agent::new(id, spec), seq: 0, epoch: 0, spawner: None });
        if let Some(p) = parent {
            self.commit(st, p, vec![Event::ChildSpawned { id, reserved }]).await?;
        }
        self.commit(st, id, vec![Event::Inbox { from: caller.clone(), content: prompt }]).await?;
        self.place(st, id).await?;
        Ok(json!({"id": id, "type": info.id()}))
    }
}

fn is_ancestor(st: &State, anc: AgentId, mut id: AgentId) -> bool {
    while let Some(p) = st.agents.get(&id).and_then(|r| r.a.spec.parent) {
        if p == anc {
            return true;
        }
        id = p;
    }
    false
}

fn summary(st: &State, id: AgentId, r: &AgentRec) -> Value {
    json!({
        "id": id,
        "type": r.a.spec.ty,
        "parent": r.a.spec.parent,
        "phase": serde_json::to_value(&r.a.phase).unwrap()["phase"],
        "pause": r.a.pause,
        "paused": r.a.is_paused(),
        "spawner": r.spawner.and_then(|c| st.spawners.get(&c)).map(|s| s.name.clone()),
        "usage": r.a.usage,
        "reserved": r.a.reserved,
        "seq": r.seq,
    })
}

fn list_types(st: &State) -> Value {
    let mut out: HashMap<String, Value> = HashMap::new();
    for s in st.spawners.values() {
        let free = s.capacity.saturating_sub(s.agents.len() as u32);
        for t in &s.types {
            let e = out.entry(t.id()).or_insert_with(|| {
                json!({"name": t.name, "id": t.id(), "description": t.description, "spawners": 0, "free": 0})
            });
            e["spawners"] = json!(e["spawners"].as_u64().unwrap() + 1);
            e["free"] = json!(e["free"].as_u64().unwrap() + free as u64);
        }
    }
    let mut v: Vec<_> = out.into_values().collect();
    v.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Value::Array(v)
}
