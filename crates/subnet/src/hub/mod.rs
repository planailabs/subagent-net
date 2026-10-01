//! The hub: sequencer of every agent's event log, type registry, placement,
//! fencing and message routing. Transports (WebSocket, MCP) are thin layers
//! over `Hub::connect`, `Hub::handle` and `Hub::op`.

pub mod auth;
pub mod blobs;
pub mod db;
pub mod ha;
pub mod http;
pub mod relay;
pub mod router;
mod watch;
pub mod switchboard;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast, mpsc};
use uuid::Uuid;

use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Agent, Budget, CallState, Effect, Event, PauseMode, Phase, Spec};
use subnet_core::chat::ToolDef;
use subnet_core::proto::{Mail, Op};

use crate::wire::{Snapshot, ToHub, ToNode};

use crate::api::{AgentSummary, Done, NodeSummary, Spawned, Transcript, TypeSummary};
use db::Db;

pub type ConnId = u64;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    Bad(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("this hub is not the leader")]
    NotLeader,
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

fn bad<T>(s: impl Into<String>) -> Result<T, HubError> {
    Err(HubError::Bad(s.into()))
}

fn no_agent<T>(id: AgentId) -> Result<T, HubError> {
    Err(HubError::NotFound(format!("no agent {id}")))
}

/// Something that happened, as streamed to subscribers (`/v1/events`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// A committed agent event. `ancestors` lists parent, grandparent, …
    Agent { agent: AgentId, ancestors: Vec<AgentId>, seq: u64, event: Event },
    /// An event from a sense.
    Sense { sense: String, node: String, id: String, at: u64, data: Value },
    /// A switchboard delivery and how each of its actions went.
    Delivery { route: String, payload: Value, outcomes: Value },
}

/// What a subscriber wants to see.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct NoticeFilter {
    /// Only this agent.
    #[serde(default)]
    pub agent: Option<AgentId>,
    /// This agent and its descendants.
    #[serde(default)]
    pub tree: Option<AgentId>,
    /// Only this sense's events.
    #[serde(default)]
    pub sense: Option<String>,
    /// Only this route's deliveries.
    #[serde(default)]
    pub route: Option<String>,
    /// Only agent notices (true) or only sense notices (false).
    #[serde(default)]
    pub agents: Option<bool>,
}

impl NoticeFilter {
    pub fn matches(&self, n: &Notice) -> bool {
        match n {
            Notice::Agent { agent, ancestors, .. } => {
                self.agents != Some(false)
                    && self.sense.is_none()
                    && self.agent.is_none_or(|a| a == *agent)
                    && self.tree.is_none_or(|t| t == *agent || ancestors.contains(&t))
            }
            Notice::Sense { sense, .. } => {
                self.agents != Some(true)
                    && self.agent.is_none()
                    && self.tree.is_none()
                    && self.route.is_none()
                    && self.sense.as_ref().is_none_or(|s| s == sense)
            }
            Notice::Delivery { route, .. } => {
                self.agents != Some(true)
                    && self.agent.is_none()
                    && self.tree.is_none()
                    && self.sense.is_none()
                    && self.route.as_ref().is_none_or(|r| r == route)
            }
        }
    }
}

struct AgentRec {
    a: Agent,
    seq: u64,
    epoch: u64,
    node: Option<ConnId>,
}

struct NodeRec {
    name: String,
    capacity: u32,
    /// Agent types (ids) this node reported it can run.
    ready: HashSet<String>,
    /// MCP types (ids) it runs, with their tools.
    mcps: HashMap<String, Vec<ToolDef>>,
    tx: mpsc::UnboundedSender<ToNode>,
    agents: HashSet<AgentId>,
    /// MCP invocations it is running for others.
    mcp_load: usize,
    /// Has answered its latest `Configure`.
    configured: bool,
    /// What it couldn't start, id → error.
    errors: std::collections::BTreeMap<String, String>,
    /// Its senses and their last problem, if any.
    senses: std::collections::BTreeMap<String, Option<String>>,
}

/// An MCP call forwarded to the node that runs the server.
struct McpPending {
    /// The node that asked; `None` for the hub's own calls (switchboard).
    from: Option<ConnId>,
    from_id: u64,
    exec: ConnId,
}

#[derive(Default)]
struct State {
    agents: HashMap<AgentId, AgentRec>,
    nodes: HashMap<ConnId, NodeRec>,
    next_conn: ConnId,
    mcp_pending: HashMap<u64, McpPending>,
    next_mcp: u64,
    /// Answers for the hub's own MCP calls.
    mcp_waiters: HashMap<u64, tokio::sync::oneshot::Sender<Result<String, String>>>,
}

pub struct Hub {
    db: Db,
    // ponytail: one global lock serialises all commits; shard per agent if it becomes the bottleneck.
    st: Mutex<State>,
    admin_token: Option<String>,
    cluster: std::sync::RwLock<auth::ClusterState>,
    /// Token hash → principal.
    tokens: std::sync::RwLock<HashMap<String, (auth::PrincipalKind, String)>>,
    mail: Notify,
    notices: broadcast::Sender<Notice>,
    /// Events between snapshots.
    snapshot_every: std::sync::atomic::AtomicU64,
    board: std::sync::Mutex<switchboard::Board>,
    board_wake: Notify,
    me: std::sync::Weak<Hub>,
    pub(crate) relay: relay::Relay,
    leader: std::sync::atomic::AtomicBool,
    leader_url: std::sync::RwLock<Option<String>>,
    leader_changed: Notify,
    fenced: std::sync::atomic::AtomicBool,
    /// Cancelled by `shutdown`: background tasks end, leadership is released.
    stop: tokio_util::sync::CancellationToken,
    /// Pre-loads lazy tools for mixtures with a `router`.
    router: std::sync::RwLock<Arc<router::Router>>,
}

/// Events only a node may propose; everything else originates at the hub.
fn node_event(e: &Event) -> bool {
    matches!(
        e,
        Event::LlmDelta { .. }
            | Event::LlmDone
            | Event::LlmAborted
            | Event::LlmFailed { .. }
            | Event::ToolResult { .. }
            | Event::ToolAborted { .. }
            | Event::Compacted { .. }
            | Event::CompactFailed { .. }
    )
}

impl Hub {
    /// `admin_token` is the bootstrap `user:root` token; without one the hub
    /// runs in open mode (everyone is root) for development.
    pub async fn open(db_url: &str, admin_token: Option<String>) -> Result<Arc<Self>, HubError> {
        let hub = Self::start(db_url, admin_token, "http://127.0.0.1").await?;
        hub.wait_leader().await;
        Ok(hub)
    }

    /// Starts a hub as a standby; it becomes leader once it wins the election
    /// (immediately, unless another hub on this database leads). `advertise`
    /// is the URL clients and nodes should use to reach this hub.
    pub async fn start(db_url: &str, admin_token: Option<String>, advertise: &str) -> Result<Arc<Self>, HubError> {
        let db = Db::connect(db_url).await?;
        if admin_token.is_none() {
            tracing::warn!("no admin token: open mode, every caller is user:root");
        }
        let hub = Arc::new_cyclic(|me| Self {
            db,
            st: Mutex::new(State::default()),
            admin_token,
            cluster: Default::default(),
            tokens: Default::default(),
            mail: Notify::new(),
            notices: broadcast::channel(4096).0,
            snapshot_every: std::sync::atomic::AtomicU64::new(200),
            board: Default::default(),
            board_wake: Notify::new(),
            me: me.clone(),
            relay: Default::default(),
            leader: Default::default(),
            leader_url: Default::default(),
            leader_changed: Notify::new(),
            fenced: Default::default(),
            stop: Default::default(),
            router: std::sync::RwLock::new(Arc::new(router::Router::default_e5())),
        });
        tokio::spawn(hub.clone().elect(db_url.to_string(), advertise.to_string()));
        tokio::spawn(hub.clone().board_timers());
        tokio::spawn(hub.clone().blob_gc());
        Ok(hub)
    }

    /// Loads everything from the database (on becoming leader).
    pub(crate) async fn load(&self) -> Result<(), HubError> {
        let mut st = State::default();
        for row in self.db.agents().await? {
            let (base, a) = match self.db.snapshot_before(row.id, u64::MAX).await? {
                Some((seq, state)) => (seq, state),
                None => (0, Agent::new(row.id, row.spec)),
            };
            let events = self.db.events(row.id, base).await?;
            let seq = base + events.len() as u64;
            st.agents.insert(row.id, AgentRec { a: a.fold(&events), seq, epoch: row.epoch, node: None });
        }
        tracing::info!(agents = st.agents.len(), "hub loaded");
        *self.st.lock().await = st;
        self.load_auth().await
    }

    fn arc(&self) -> Arc<Hub> {
        self.me.upgrade().expect("hub is alive while its methods run")
    }

    /// For in-process nodes, which are trusted.
    pub(crate) fn token(&self) -> Option<String> {
        self.admin_token.clone()
    }

    /// How many events between snapshots (default 200).
    pub fn set_snapshot_every(&self, n: u64) {
        self.snapshot_every.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }

    // ---------- node connections ----------

    /// What a node runs: its part of the applied cluster (empty if the cluster
    /// doesn't declare it; only possible in open mode).
    fn node_config(&self, name: &str) -> subnet_cluster::NodeConfig {
        self.cluster.read().unwrap().spec.node_config(name).unwrap_or_else(|| subnet_cluster::NodeConfig {
            node: name.to_string(),
            capacity: 16,
            agents: vec![],
            mcps: vec![],
            senses: Default::default(),
            relay_out: vec![],
            relay_in: vec![],
        })
    }

    pub async fn connect(&self, hello: ToHub) -> Result<(ConnId, mpsc::UnboundedReceiver<ToNode>), String> {
        let ToHub::Hello { name, token } = hello else {
            return Err("expected hello".into());
        };
        if !self.node_ok(&name, token.as_deref()) {
            return Err(format!("bad token or undeclared node {name:?}"));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let config = self.node_config(&name);
        let mut st = self.st.lock().await;
        if st.nodes.values().any(|n| n.name == name) {
            return Err(format!("a node named {name:?} is already connected"));
        }
        let conn = st.next_conn;
        st.next_conn += 1;
        tracing::info!(conn, %name, capacity = config.capacity, "node connected");
        let _ = tx.send(ToNode::Welcome);
        let capacity = config.capacity;
        let _ = tx.send(ToNode::Configure { config });
        st.nodes.insert(
            conn,
            NodeRec {
                name,
                capacity,
                ready: HashSet::new(),
                mcps: HashMap::new(),
                tx,
                agents: HashSet::new(),
                mcp_load: 0,
                configured: false,
                errors: Default::default(),
                senses: Default::default(),
            },
        );
        Ok((conn, rx))
    }

    /// Sends every node its current configuration (after an apply).
    async fn reconfigure_nodes(&self) {
        let st = self.st.lock().await;
        for n in st.nodes.values() {
            let _ = n.tx.send(ToNode::Configure { config: self.node_config(&n.name) });
        }
    }

    pub async fn disconnect(&self, conn: ConnId) {
        let mut st = self.st.lock().await;
        let Some(s) = st.nodes.remove(&conn) else { return };
        tracing::warn!(conn, name = %s.name, agents = s.agents.len(), "node gone, reassigning its agents");
        for id in s.agents {
            if let Some(r) = st.agents.get_mut(&id) {
                r.node = None;
            }
        }
        // MCP calls it was running fail; calls it made are dropped.
        let gone: Vec<u64> =
            st.mcp_pending.iter().filter(|(_, p)| p.exec == conn || p.from == Some(conn)).map(|(h, _)| *h).collect();
        for h in gone {
            let p = st.mcp_pending.remove(&h).unwrap();
            if p.exec != conn {
                continue;
            }
            let err = Err("the node running the tool went away".to_string());
            match p.from {
                Some(from) => {
                    if let Some(n) = st.nodes.get(&from) {
                        let _ = n.tx.send(ToNode::McpReply { id: p.from_id, result: err });
                    }
                }
                None => {
                    if let Some(w) = st.mcp_waiters.remove(&h) {
                        let _ = w.send(err);
                    }
                }
            }
        }
        if let Err(e) = self.place_pending(&mut st).await {
            tracing::error!(error = %e, "placement after disconnect failed");
        }
    }

    pub async fn handle(&self, conn: ConnId, msg: ToHub) -> Result<(), HubError> {
        match msg {
            ToHub::Hello { .. } => bad("duplicate hello"),
            ToHub::Ready { agents, mcps } => {
                let mut st = self.st.lock().await;
                let Some(n) = st.nodes.get_mut(&conn) else { return Ok(()) };
                n.errors = agents
                    .iter()
                    .filter_map(|a| Some((a.id.clone(), a.error.clone()?)))
                    .chain(mcps.iter().filter_map(|m| Some((m.id.clone(), m.error.clone()?))))
                    .collect();
                n.ready = agents.into_iter().filter(|a| a.error.is_none()).map(|a| a.id).collect();
                n.mcps = mcps.into_iter().filter(|m| m.error.is_none()).map(|m| (m.id, m.tools)).collect();
                n.configured = true;
                tracing::info!(node = %n.name, agents = ?n.ready, mcps = ?n.mcps.keys().collect::<Vec<_>>(), "node ready");
                self.place_pending(&mut st).await?;
                drop(st);
                self.sync_residents().await;
                Ok(())
            }
            ToHub::McpCall { id, agent, epoch, mcp, tool, args } => {
                let mut st = self.st.lock().await;
                if !self.owns(&st, conn, agent, epoch) {
                    return Ok(());
                }
                let allowed = st.agents[&agent].a.spec.mcp.values().any(|m| *m == mcp);
                let exec = st
                    .nodes
                    .iter()
                    .filter(|(_, n)| n.mcps.contains_key(&mcp))
                    .min_by_key(|(c, n)| (n.mcp_load, **c))
                    .map(|(c, _)| *c);
                let reply = |st: &State, result: Result<String, String>| {
                    if let Some(n) = st.nodes.get(&conn) {
                        let _ = n.tx.send(ToNode::McpReply { id, result });
                    }
                };
                match (allowed, exec) {
                    (false, _) => reply(&st, Err(format!("mcp {mcp} is not part of this agent's mixture"))),
                    (true, None) => reply(&st, Err(format!("no node runs mcp {mcp} right now"))),
                    (true, Some(exec)) => {
                        let hub_id = st.next_mcp;
                        st.next_mcp += 1;
                        st.mcp_pending.insert(hub_id, McpPending { from: Some(conn), from_id: id, exec });
                        let tenant = st.agents[&agent].a.spec.tenant.clone();
                        let n = st.nodes.get_mut(&exec).unwrap();
                        n.mcp_load += 1;
                        let _ = n.tx.send(ToNode::McpInvoke { id: hub_id, mcp, tool, args, tenant });
                    }
                }
                Ok(())
            }
            ToHub::McpCancel { id } => {
                let mut st = self.st.lock().await;
                let found =
                    st.mcp_pending.iter().find(|(_, p)| p.from == Some(conn) && p.from_id == id).map(|(h, _)| *h);
                if let Some(h) = found {
                    let p = st.mcp_pending.remove(&h).unwrap();
                    if let Some(n) = st.nodes.get_mut(&p.exec) {
                        n.mcp_load = n.mcp_load.saturating_sub(1);
                        let _ = n.tx.send(ToNode::McpAbort { id: h });
                    }
                }
                Ok(())
            }
            ToHub::SenseEvent { sense, id, at, data } => {
                let node = self.st.lock().await.nodes.get(&conn).map(|n| n.name.clone()).unwrap_or_default();
                self.arc().sense_event(node, subnet_switchboard::SenseEvent { id, sense, at, data });
                Ok(())
            }
            ToHub::Blob { hash, mime, base64 } => {
                let data = blobs::decode_b64(&base64)?;
                self.put_blob_checked(&hash, &mime, &data).await
            }
            ToHub::SenseStatus { sense, error } => {
                let mut st = self.st.lock().await;
                if let Some(n) = st.nodes.get_mut(&conn) {
                    n.senses.insert(sense, error);
                }
                Ok(())
            }
            ToHub::McpResult { id, result } => {
                let mut st = self.st.lock().await;
                let Some(p) = st.mcp_pending.remove(&id) else { return Ok(()) };
                if p.exec != conn {
                    return bad("mcp result from the wrong node");
                }
                if let Some(n) = st.nodes.get_mut(&conn) {
                    n.mcp_load = n.mcp_load.saturating_sub(1);
                }
                match p.from {
                    Some(from) => {
                        if let Some(n) = st.nodes.get(&from) {
                            let _ = n.tx.send(ToNode::McpReply { id: p.from_id, result });
                        }
                    }
                    None => {
                        if let Some(w) = st.mcp_waiters.remove(&id) {
                            let _ = w.send(result);
                        }
                    }
                }
                Ok(())
            }
            ToHub::Propose { agent, epoch, events } => {
                let mut st = self.st.lock().await;
                if !self.owns(&st, conn, agent, epoch) {
                    return Ok(());
                }
                if let Some(e) = events.iter().find(|e| !node_event(e)) {
                    return bad(format!("node may not propose {e:?}"));
                }
                self.commit(&mut st, agent, events).await
            }
            ToHub::Request { id, agent, epoch, op } => {
                let owns = self.owns(&*self.st.lock().await, conn, agent, epoch);
                let result = if owns { self.op(&Addr::Agent(agent), op).await } else { Err("stale epoch".into()) };
                self.to_node(conn, ToNode::Reply { id, result }).await;
                Ok(())
            }
        }
    }

    /// Fencing: only the current owner at the current epoch may write. A stale
    /// writer is told to stop.
    fn owns(&self, st: &State, conn: ConnId, agent: AgentId, epoch: u64) -> bool {
        let ok = st.agents.get(&agent).is_some_and(|r| r.epoch == epoch && r.node == Some(conn));
        if !ok {
            tracing::warn!(conn, %agent, epoch, "rejected write from non-owner");
            if let Some(s) = st.nodes.get(&conn) {
                let _ = s.tx.send(ToNode::Revoke { agent });
            }
        }
        ok
    }

    async fn to_node(&self, conn: ConnId, msg: ToNode) {
        if let Some(s) = self.st.lock().await.nodes.get(&conn) {
            let _ = s.tx.send(msg);
        }
    }

    // ---------- log and placement ----------

    async fn commit(&self, st: &mut State, id: AgentId, events: Vec<Event>) -> Result<(), HubError> {
        self.run(st, vec![Work::Commit(id, events)]).await
    }

    async fn place_pending(&self, st: &mut State) -> Result<(), HubError> {
        let pending = st.agents.iter().filter(|(_, r)| r.node.is_none()).map(|(id, _)| Work::Place(*id)).collect();
        self.run(st, pending).await
    }

    /// Processes commits and placements until nothing follows from them.
    ///
    /// A commit appends events to an agent's log, updates the replica,
    /// forwards the events to the owning node and routes resulting reports.
    /// Reports are routed here, from the hub's replica, so each is delivered
    /// exactly once whatever happens to the node. An unassigned agent that
    /// now has work is placed.
    async fn run(&self, st: &mut State, work: Vec<Work>) -> Result<(), HubError> {
        let mut work = std::collections::VecDeque::from(work);
        while let Some(w) = work.pop_front() {
            match w {
                Work::Commit(id, events) => self.commit_one(st, id, events, &mut work).await?,
                Work::Place(id) => self.place_one(st, id, &mut work).await?,
            }
        }
        Ok(())
    }

    async fn commit_one(
        &self,
        st: &mut State,
        id: AgentId,
        events: Vec<Event>,
        work: &mut VecDeque<Work>,
    ) -> Result<(), HubError> {
        if events.is_empty() {
            return Ok(());
        }
        let Some(r) = st.agents.get_mut(&id) else { return bad(format!("no agent {id}")) };
        if !self.db.append(id, r.seq + 1, &events).await? {
            return Err(self.fence());
        }
        let before = r.seq;
        let ends_with_recovery = events.last() == Some(&Event::Recovered);
        let ancestors = {
            let mut v = vec![];
            let mut p = r.a.spec.parent;
            while let Some(x) = p {
                v.push(x);
                p = st.agents.get(&x).and_then(|r| r.a.spec.parent);
            }
            v
        };
        let r = st.agents.get_mut(&id).unwrap();
        let mut reports = vec![];
        for event in events {
            r.seq += 1;
            for fx in r.a.apply(&event) {
                if let Effect::Report { to, status, content } = fx {
                    reports.push((to, status, content));
                }
            }
            let seq = r.seq;
            if let Some(s) = r.node.and_then(|c| st.nodes.get(&c)) {
                let _ = s.tx.send(ToNode::Commit { agent: id, seq, event: event.clone() });
            }
            let _ = self.notices.send(Notice::Agent { agent: id, ancestors: ancestors.clone(), seq, event });
        }
        // A snapshot each time the log crosses a multiple of `snapshot_every`.
        // Never right after `Recovered`: an assignment needs one event after its snapshot.
        let every = self.snapshot_every.load(std::sync::atomic::Ordering::Relaxed);
        if before / every != r.seq / every && !ends_with_recovery {
            self.db.put_snapshot(id, r.seq, &r.a).await?;
        }
        let was_on = r.node;
        if r.a.phase == Phase::Cancelled {
            unassign(st, id);
        }
        let r = &st.agents[&id];
        if r.node.is_none() && wants_runner(&r.a) {
            work.push_back(Work::Place(id));
        } else if let Some(conn) = was_on
            && !wants_runner(&r.a)
            && let Some(s) = st.nodes.get(&conn)
        {
            // Its slot can be taken over by an agent waiting for one.
            // ponytail: O(agents) scan per idle transition; index pending agents by type if it shows up.
            work.extend(
                st.agents
                    .iter()
                    .filter(|(_, p)| p.node.is_none() && s.ready.contains(&p.a.spec.ty) && wants_runner(&p.a))
                    .map(|(p, _)| Work::Place(*p)),
            );
        }
        let parent = r.a.spec.parent;
        for (to, status, content) in reports {
            for addr in to {
                match addr {
                    Addr::Agent(p) if Some(p) == parent => {
                        work.push_back(Work::Commit(
                            p,
                            vec![Event::ChildReport { id, status, content: content.clone() }],
                        ));
                    }
                    Addr::Agent(x) if st.agents.contains_key(&x) => {
                        let ev = Event::Inbox { from: Addr::Agent(id), content: content.clone(), reply: true };
                        work.push_back(Work::Commit(x, vec![ev]));
                    }
                    Addr::Agent(x) => tracing::warn!(from = %id, to = %x, "report to unknown agent dropped"),
                    Addr::Route(rt) => {
                        let mail = Mail { from: Addr::Agent(id), content: content.clone(), status: Some(status) };
                        self.put_mail(&Addr::Route(rt.clone()), &mail).await?;
                        self.arc().route_agent_done(&rt, id);
                    }
                    other => {
                        let mail = Mail { from: Addr::Agent(id), content: content.clone(), status: Some(status) };
                        self.put_mail(&other, &mail).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Assigns an agent that wants a runner to the least-loaded live node
    /// offering its exact type, evicting an agent with nothing to do if every
    /// such node is full. Without a node the agent stays pending until
    /// one connects; agents with nothing to do stay unassigned and cost nothing.
    async fn place_one(&self, st: &mut State, id: AgentId, work: &mut VecDeque<Work>) -> Result<(), HubError> {
        let r = &st.agents[&id];
        if r.node.is_some() || !wants_runner(&r.a) {
            return Ok(());
        }
        let ty = r.a.spec.ty.clone();
        let offering = |s: &NodeRec| s.ready.contains(&ty);
        let free = st
            .nodes
            .iter()
            .filter(|(_, s)| offering(s) && (s.agents.len() as u32) < s.capacity)
            .min_by_key(|(c, s)| (s.agents.len(), **c))
            .map(|(c, _)| *c);
        let conn = match free {
            Some(c) => c,
            None => {
                let victim = st
                    .nodes
                    .iter()
                    .filter(|(_, s)| offering(s))
                    .find_map(|(c, s)| s.agents.iter().find(|v| !wants_runner(&st.agents[v].a)).map(|v| (*c, *v)));
                let Some((c, v)) = victim else {
                    tracing::debug!(%id, %ty, "no node for agent, pending");
                    return Ok(());
                };
                tracing::info!(agent = %v, "evicting idle agent to make room");
                unassign(st, v);
                c
            }
        };
        // Whatever ran before is gone; log that so every replica agrees.
        self.commit_one(st, id, vec![Event::Recovered], work).await?;
        work.retain(|w| !matches!(w, Work::Place(p) if *p == id));
        if !wants_runner(&st.agents[&id].a) {
            return Ok(()); // e.g. recovery ran it out of budget
        }
        let r = st.agents.get_mut(&id).unwrap();
        let epoch = r.epoch + 1;
        self.db.set_epoch(id, epoch).await?;
        let snapshot = self.db.snapshot_before(id, r.seq).await?.map(|(seq, state)| Box::new(Snapshot { seq, state }));
        let events = self.db.events(id, snapshot.as_ref().map_or(0, |s| s.seq)).await?;
        r.epoch = epoch;
        r.node = Some(conn);
        let s = st.nodes.get_mut(&conn).unwrap();
        s.agents.insert(id);
        tracing::info!(%id, node = %s.name, epoch, "assigned");
        let _ = s.tx.send(ToNode::Assign { agent: id, epoch, spec: r.a.spec.clone(), snapshot, events });
        Ok(())
    }

    /// An MCP call of the hub's own (switchboard `mcp` deliveries).
    pub(crate) async fn mcp_from_hub(&self, mcp: &str, tool: &str, args: Value) -> Result<String, String> {
        let rx = {
            let mut st = self.st.lock().await;
            let exec = st
                .nodes
                .iter()
                .filter(|(_, n)| n.mcps.contains_key(mcp))
                .min_by_key(|(c, n)| (n.mcp_load, **c))
                .map(|(c, _)| *c)
                .ok_or_else(|| format!("no node runs mcp {mcp} right now"))?;
            let id = st.next_mcp;
            st.next_mcp += 1;
            let (tx, rx) = tokio::sync::oneshot::channel();
            st.mcp_pending.insert(id, McpPending { from: None, from_id: id, exec });
            st.mcp_waiters.insert(id, tx);
            let n = st.nodes.get_mut(&exec).unwrap();
            n.mcp_load += 1;
            let _ = n.tx.send(ToNode::McpInvoke { id, mcp: mcp.into(), tool: tool.into(), args, tenant: None });
            rx
        };
        rx.await.unwrap_or_else(|_| Err("mcp call dropped".into()))
    }

    async fn put_mail(&self, to: &Addr, mail: &Mail) -> Result<(), HubError> {
        self.db.put_mail(to, mail).await?;
        self.mail.notify_waiters();
        Ok(())
    }

    // ---------- operations ----------

    /// Built-in tool calls of agents (and tests): dispatches to the typed
    /// operations and returns their result as JSON.
    pub async fn op(&self, caller: &Addr, op: Op) -> Result<Value, String> {
        fn j<T: Serialize>(r: Result<T, HubError>) -> Result<Value, String> {
            r.map(|v| serde_json::to_value(v).unwrap()).map_err(|e| e.to_string())
        }
        match op {
            Op::Spawn { ty, prompt, tenant } => j(self.spawn_for(caller, &ty, prompt, tenant).await),
            Op::Send { to, content } => j(self.send(caller, to, content).await),
            Op::ListAgents => j(Ok(self.list_agents().await)),
            Op::ListTypes => j(Ok(self.list_types().await)),
            Op::Pause { id, mode, tree } => j(self.pause(caller, id, mode, tree).await),
            Op::Resume { id, tree } => j(self.resume(caller, id, tree).await),
            Op::Cancel { id } => j(self.cancel(caller, id).await),
            Op::Approve { id, call_id, approved } => j(self.approve(caller, id, call_id, approved).await),
            Op::Fork { id, at, tree } => j(self.fork(caller, id, at, tree).await),
            Op::Transcript { id } => j(self.transcript(id).await),
            Op::WaitInbox { timeout_ms } => j(self.wait_inbox(caller, timeout_ms).await),
            Op::MailboxTake { name, max } => j(self.mailbox(caller, &name, max, true).await),
            Op::MailboxPeek { name, max } => j(self.mailbox(caller, &name, max, false).await),
            Op::BlobGet { reference } => j(self.blob_for_model(&reference).await),
            Op::BlobRaw { reference } => j(self.get_blob(&reference).await.map(|(mime, data)| serde_json::json!({"mime": mime, "base64": blobs::encode_b64(&data)}))),
        }
    }

    pub async fn list_agents(&self) -> Vec<AgentSummary> {
        let st = self.st.lock().await;
        let c = self.cluster.read().unwrap().spec.clone();
        let mut v: Vec<_> = st.agents.iter().map(|(id, r)| summary(&st, &c, *id, r)).collect();
        v.sort_by_key(|a| a.id);
        v
    }

    pub async fn list_nodes(&self) -> Vec<NodeSummary> {
        let st = self.st.lock().await;
        let mut v: Vec<_> = st
            .nodes
            .values()
            .map(|n| {
                let mut ready: Vec<_> = n.ready.iter().cloned().collect();
                ready.sort();
                let mut mcps: Vec<_> = n.mcps.keys().cloned().collect();
                mcps.sort();
                NodeSummary {
                    name: n.name.clone(),
                    capacity: n.capacity,
                    running: n.agents.len() as u32,
                    configured: n.configured,
                    agents: ready,
                    mcps,
                    errors: n.errors.clone(),
                    senses: n.senses.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub async fn list_types(&self) -> Vec<TypeSummary> {
        let c = self.cluster.read().unwrap().spec.clone();
        list_types(&*self.st.lock().await, &c)
    }

    pub async fn transcript(&self, id: AgentId) -> Result<Transcript, HubError> {
        let st = self.st.lock().await;
        let Some(r) = st.agents.get(&id) else { return no_agent(id) };
        Ok(Transcript {
            summary: summary(&st, &self.cluster.read().unwrap().spec.clone(), id, r),
            messages: r.a.messages.clone(),
            partial: (!r.a.acc.is_empty()).then(|| r.a.acc.partial()),
            inbox: r.a.inbox.iter().cloned().collect(),
        })
    }

    pub async fn send(&self, caller: &Addr, to: Addr, content: String) -> Result<Done, HubError> {
        let to = self.resolve(to).await?;
        let mut st = self.st.lock().await;
        match &to {
            Addr::Agent(id) if st.agents.contains_key(id) => {
                let mut events = self.route_tools(&st.agents[id].a, &content).await;
                events.push(Event::Inbox { from: caller.clone(), content, reply: false });
                self.commit(&mut st, *id, events).await?
            }
            Addr::Agent(id) => return no_agent(*id),
            other => self.put_mail(other, &Mail { from: caller.clone(), content, status: None }).await?,
        }
        Ok(Done::OK)
    }

    /// Replaces the tool router (another embedder, tests).
    pub fn set_router(&self, r: router::Router) {
        *self.router.write().unwrap() = Arc::new(r);
    }

    /// `ToolsLoaded` for the lazy tools matching a message, if the agent's
    /// mixture has a router.
    // ponytail: embeds under the hub lock (one short query per message, tools are cached); move out if it shows up in latency.
    async fn route_tools(&self, a: &Agent, text: &str) -> Vec<Event> {
        let def = {
            let c = self.cluster.read().unwrap();
            a.spec.mixture.as_ref().and_then(|m| c.spec.mixtures.get(m)).and_then(|m| m.router.clone())
        };
        let Some(def) = def else { return vec![] };
        let tools: Vec<ToolDef> = a.unloaded().into_iter().cloned().collect();
        let router = self.router.read().unwrap().clone();
        let names = router.pick(&def, tools, text.to_string()).await;
        if names.is_empty() {
            return vec![];
        }
        tracing::debug!(agent = %a.id, tools = ?names, "router pre-loads");
        vec![Event::ToolsLoaded { names }]
    }

    /// `resident:<name>` → the resident's agent; other addresses unchanged.
    pub async fn resolve(&self, a: Addr) -> Result<Addr, HubError> {
        match a {
            Addr::Resident(n) => match self.db.resident(&n).await? {
                Some(id) => Ok(Addr::Agent(id)),
                None => Err(HubError::NotFound(format!("no resident {n:?}"))),
            },
            other => Ok(other),
        }
    }

    pub async fn pause(&self, caller: &Addr, id: AgentId, mode: PauseMode, tree: bool) -> Result<Done, HubError> {
        self.control(caller, id, tree, Event::PauseRequested { mode }).await
    }

    pub async fn resume(&self, caller: &Addr, id: AgentId, tree: bool) -> Result<Done, HubError> {
        self.control(caller, id, tree, Event::Resumed).await
    }

    pub async fn cancel(&self, caller: &Addr, id: AgentId) -> Result<Done, HubError> {
        let done = self.control(caller, id, true, Event::Cancelled).await?;
        // A cancelled resident starts afresh.
        if self.db.residents().await?.iter().any(|(_, r)| *r == id) {
            // Boxed: syncing cancels residents dropped from the cluster.
            Box::pin(self.sync_residents()).await;
        }
        Ok(done)
    }

    pub async fn approve(&self, caller: &Addr, id: AgentId, call_id: String, approved: bool) -> Result<Done, HubError> {
        self.control(caller, id, false, Event::Approval { call_id, approved }).await
    }

    async fn control(&self, caller: &Addr, id: AgentId, tree: bool, ev: Event) -> Result<Done, HubError> {
        let mut st = self.st.lock().await;
        for id in self.targets(&st, caller, id, tree)? {
            self.commit(&mut st, id, vec![ev.clone()]).await?;
        }
        Ok(Done::OK)
    }

    /// Copies an agent's history (the first `at` events) into a new agent.
    /// Without `tree` the copy has no children; with `tree` every child it
    /// had spawned by then is forked too (recursively, with their full logs),
    /// and agent ids are remapped throughout the copied events.
    pub async fn fork(&self, caller: &Addr, id: AgentId, at: Option<u64>, tree: bool) -> Result<Spawned, HubError> {
        if matches!(caller, Addr::Agent(_)) {
            return Err(HubError::Forbidden("agents may not fork".into()));
        }
        let mut st = self.st.lock().await;
        self.copy(&mut st, id, at, tree, false).await
    }

    /// Moves an agent onto the current version of its type (and mixture's
    /// MCP servers): its whole history is copied into a new agent whose spec
    /// is built from the cluster as it is now (keeping its parent, budget and
    /// tenant), the old agent is cancelled, and a resident it was follows the
    /// copy. Only roots: with `tree` its children move too; without, they're
    /// cancelled.
    /// An agent of an older version is otherwise never resumed.
    pub async fn upgrade(&self, caller: &Addr, id: AgentId, tree: bool) -> Result<Spawned, HubError> {
        if matches!(caller, Addr::Agent(_)) {
            return Err(HubError::Forbidden("agents may not upgrade agents".into()));
        }
        let mut st = self.st.lock().await;
        if let Some(p) = st.agents.get(&id).and_then(|r| r.a.spec.parent) {
            return bad(format!("{id} is a child of {p}: upgrade the root of its tree (with tree)"));
        }
        let new = self.copy(&mut st, id, None, tree, true).await?;
        // The old tree stops without reporting: its work goes on in the copy.
        for old in self.targets(&st, caller, id, true)? {
            self.commit(&mut st, old, vec![Event::Superseded { by: new.id }]).await?;
        }
        drop(st);
        for (name, rid) in self.db.residents().await? {
            if rid == id {
                self.db.set_resident(&name, new.id).await?;
                tracing::info!(resident = %name, from = %id, to = %new.id, ty = %new.ty, "resident upgraded");
            }
        }
        Ok(new)
    }

    /// The spec `old` would get if it were spawned now: the current type
    /// and MCP servers, with its own parent, budget and tenant.
    fn current_spec(&self, st: &State, old: &Spec) -> Result<Spec, HubError> {
        let name = old.mixture.clone().unwrap_or_else(|| old.ty.split('@').next().unwrap_or_default().to_string());
        let (mut spec, _) = self.spec_for(st, &name, None, old.tenant.clone())?;
        spec.parent = old.parent;
        spec.budget = old.budget.clone();
        Ok(spec)
    }

    /// Copies an agent's history (the first `at` events) into a new agent,
    /// with its spec as it was (fork) or as it would be now (`fresh`, upgrade).
    async fn copy(&self, st: &mut State, id: AgentId, at: Option<u64>, tree: bool, fresh: bool) -> Result<Spawned, HubError> {
        let Some(r) = st.agents.get(&id) else { return no_agent(id) };
        let mut root_events = self.db.events(id, 0).await?;
        root_events.truncate(at.unwrap_or(r.seq) as usize);
        let spawned = |evs: &[Event]| -> Vec<AgentId> {
            evs.iter().filter_map(|e| if let Event::ChildSpawned { id, .. } = e { Some(*id) } else { None }).collect()
        };
        // (old id, events) for the root and, with `tree`, every descendant.
        let mut subtree = vec![(id, root_events)];
        if tree {
            let mut i = 0;
            while i < subtree.len() {
                for c in spawned(&subtree[i].1) {
                    if st.agents.contains_key(&c) && !subtree.iter().any(|(x, _)| *x == c) {
                        subtree.push((c, self.db.events(c, 0).await?));
                    }
                }
                i += 1;
            }
        } else {
            subtree[0].1.retain(|e| !matches!(e, Event::ChildSpawned { .. } | Event::ChildReport { .. }));
        }
        let map: HashMap<AgentId, AgentId> = subtree.iter().map(|(old, _)| (*old, Uuid::new_v4())).collect();
        let remap = |e: &Event| -> Event {
            let mut j = serde_json::to_string(e).unwrap();
            for (old, new) in &map {
                j = j.replace(&old.to_string(), &new.to_string());
            }
            serde_json::from_str(&j).expect("remapped event parses")
        };
        // Built before anything is created, so a missing type fails cleanly.
        let mut specs = HashMap::new();
        for (old, _) in &subtree {
            let was = &st.agents[old].a.spec;
            specs.insert(*old, if fresh { self.current_spec(st, was)? } else { was.clone() });
        }
        for (old, events) in &subtree {
            let new = map[old];
            let mut spec = specs.remove(old).unwrap();
            spec.parent = if old == &id { None } else { spec.parent.and_then(|p| map.get(&p).copied()) };
            self.db.create_agent(new, &spec).await?;
            st.agents.insert(new, AgentRec { a: Agent::new(new, spec), seq: 0, epoch: 0, node: None });
            let events: Vec<Event> = events.iter().map(remap).collect();
            self.restore(st, new, events).await?;
        }
        let root = map[&id];
        Ok(Spawned { id: root, ty: st.agents[&root].a.spec.ty.clone() })
    }

    /// Appends copied history: the replica folds it, but its effects (reports)
    /// already happened for the original and aren't repeated.
    async fn restore(&self, st: &mut State, id: AgentId, events: Vec<Event>) -> Result<(), HubError> {
        if !events.is_empty() {
            let r = st.agents.get_mut(&id).unwrap();
            if !self.db.append(id, r.seq + 1, &events).await? {
                return Err(self.fence());
            }
            r.seq += events.len() as u64;
            r.a = std::mem::replace(&mut r.a, Agent::new(id, Spec::of_type(""))).fold(&events);
        }
        self.run(st, vec![Work::Place(id)]).await
    }

    pub async fn wait_inbox(&self, caller: &Addr, timeout_ms: Option<u64>) -> Result<Vec<Mail>, HubError> {
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
                return Ok(mail);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(vec![]);
            }
        }
    }

    /// Resolves targets of a control op and checks the caller may act on them:
    /// users and clients may act on anyone, agents only on their descendants.
    fn targets(&self, st: &State, caller: &Addr, id: AgentId, tree: bool) -> Result<Vec<AgentId>, HubError> {
        if !st.agents.contains_key(&id) {
            return no_agent(id);
        }
        if let Addr::Agent(me) = caller
            && !is_ancestor(st, *me, id)
        {
            return Err(HubError::Forbidden(format!("{id} is not a descendant of {caller}")));
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

    pub async fn spawn(&self, caller: &Addr, ty: &str, prompt: String) -> Result<Spawned, HubError> {
        self.spawn_for(caller, ty, prompt, None).await
    }

    /// Spawns for a tenant (whose per-tenant MCP servers the agent and its
    /// descendants use). Agents can't pick one: their children inherit theirs.
    pub async fn spawn_for(&self, caller: &Addr, ty: &str, prompt: String, tenant: Option<String>) -> Result<Spawned, HubError> {
        if tenant.is_some() && matches!(caller, Addr::Agent(_)) {
            return Err(HubError::Forbidden("agents' children inherit their tenant".into()));
        }
        let mut st = self.st.lock().await;
        self.spawn_in(&mut st, caller, ty, prompt, tenant).await
    }

    /// Builds the spec for spawning `name` (a mixture or a bare agent type)
    /// under `parent`, fixing its tools from the MCP servers nodes run now.
    fn spec_for(&self, st: &State, name: &str, parent: Option<AgentId>, tenant: Option<String>) -> Result<(Spec, u64), HubError> {
        let c = self.cluster.read().unwrap().spec.clone();
        let (agent, mixture, mcps) = match (c.mixtures.get(name), c.agents.get(name)) {
            (Some(m), _) => (m.agent.clone(), Some(name.to_string()), m.mcp.clone()),
            (None, Some(_)) => (name.to_string(), None, vec![]),
            _ => return Err(HubError::NotFound(format!("no mixture or agent type {name:?}"))),
        };
        let def = &c.agents[&agent];
        let ty = c.agent_id(&agent).unwrap();
        if !st.nodes.values().any(|n| n.ready.contains(&ty)) {
            return bad(format!("no live node offers type {agent:?} ({ty})"));
        }
        let mut tools = subnet_core::tools::builtin_tools();
        let mut mcp = std::collections::BTreeMap::new();
        let mut idempotent = vec![];
        let mut lazy = vec![];
        for m in &mcps {
            let id = c.mcp_id(m).unwrap();
            let Some(listed) = st.nodes.values().find_map(|n| n.mcps.get(&id)) else {
                return bad(format!("no live node runs mcp {m:?}, needed by {name:?}"));
            };
            tools.extend(listed.iter().map(|t| ToolDef { name: format!("{m}.{}", t.name), ..t.clone() }));
            if c.mcps[m].lazy {
                lazy.extend(listed.iter().map(|t| format!("{m}.{}", t.name)));
            }
            idempotent.extend(c.mcps[m].idempotent.iter().map(|t| format!("{m}.{t}")));
            mcp.insert(m.clone(), id);
        }
        let (budget, reserved) = match parent {
            Some(p) => {
                let r = &st.agents[&p];
                let parent_agent = r.a.spec.ty.split('@').next().unwrap_or_default();
                let spawns = c.agents.get(parent_agent).map(|a| a.spawns.clone()).unwrap_or_default();
                if !spawns.iter().any(|s| s == name) {
                    return Err(HubError::Forbidden(format!("type {parent_agent} may not spawn {name}")));
                }
                let pb = &r.a.spec.budget;
                if pb.max_depth == 0 {
                    return bad("depth budget exhausted");
                }
                if r.a.children.len() as u32 >= pb.max_children {
                    return bad(format!("child budget exhausted ({} children)", pb.max_children));
                }
                // A child under a limited parent gets at most half of what is
                // left (an unlimited type exactly that), so the parent is never
                // starved by its children: it still has to read their answers.
                let max_tokens = match (def.budget.max_tokens, r.a.remaining_tokens()) {
                    (Some(t), Some(left)) => Some(t.min(left / 2)),
                    (t, None) => t,
                    (None, Some(left)) => Some(left / 2),
                };
                if max_tokens == Some(0) {
                    return bad("token budget exhausted");
                }
                // Halving never reaches zero: refuse children too small to be useful.
                if let (Some(t), Some(got)) = (def.budget.max_tokens, max_tokens)
                    && got < t / 10
                {
                    return bad(format!("token budget exhausted: a {name} would get {got} tokens (it needs at least {}); work with what you have", t / 10));
                }
                let budget = Budget {
                    cached_percent: def.budget.cached_percent,
                    max_tokens,
                    max_depth: def.budget.max_depth.min(pb.max_depth - 1),
                    max_children: def.budget.max_children,
                };
                // Only a limited parent reserves; an unlimited one has nothing to carve from.
                let reserved = r.a.spec.budget.max_tokens.and(max_tokens).unwrap_or(0);
                (budget, reserved)
            }
            None => (def.budget.clone(), 0),
        };
        let compact = def.compact.spec(&def.executor);
        let tenant = match parent {
            Some(p) => st.agents[&p].a.spec.tenant.clone(),
            None => tenant,
        };
        let spec = Spec { ty, mixture, parent, budget, approve: def.approve.clone(), mcp, tools, idempotent, lazy, compact, tenant, vision: def.vision.clone() };
        Ok((spec, reserved))
    }

    async fn spawn_in(&self, st: &mut State, caller: &Addr, name: &str, prompt: String, tenant: Option<String>) -> Result<Spawned, HubError> {
        let parent = match caller {
            Addr::Agent(p) => Some(*p),
            _ => None,
        };
        let (spec, reserved) = self.spec_for(st, name, parent, tenant)?;
        let ty = spec.ty.clone();
        let id = Uuid::new_v4();
        self.db.create_agent(id, &spec).await?;
        st.agents.insert(id, AgentRec { a: Agent::new(id, spec), seq: 0, epoch: 0, node: None });
        if let Some(p) = parent {
            self.commit(st, p, vec![Event::ChildSpawned { id, reserved }]).await?;
        }
        let mut events = self.route_tools(&st.agents[&id].a, &prompt).await;
        events.push(Event::Inbox { from: caller.clone(), content: prompt, reply: false });
        self.commit(st, id, events).await?;
        Ok(Spawned { id, ty })
    }

    /// Creates residents the cluster declares but that don't exist yet (once
    /// their types are available) and cancels residents it no longer declares.
    pub async fn sync_residents(&self) {
        let c = self.cluster();
        let existing = match self.db.residents().await {
            Ok(r) => r,
            Err(e) => return tracing::error!(error = %e, "loading residents failed"),
        };
        for (name, id) in &existing {
            if !c.spec.residents.contains_key(name) {
                tracing::info!(resident = %name, agent = %id, "resident removed from the cluster, cancelling");
                // No longer a resident first, so the cancel doesn't recreate it.
                if let Err(e) = self.db.remove_resident(name).await {
                    tracing::error!(error = %e, "removing resident failed");
                    continue;
                }
                let _ = self.cancel(&Addr::root(), *id).await;
            }
        }
        // A resident whose type (or MCP servers) the cluster changed moves
        // onto the new version once a node offers it, keeping its history.
        for (name, id) in &existing {
            if !c.spec.residents.contains_key(name) {
                continue;
            }
            let ready = {
                let st = self.st.lock().await;
                st.agents.get(id).is_some_and(|r| outdated(&c.spec, &r.a.spec) && !matches!(r.a.phase, subnet_core::agent::Phase::Cancelled) && self.current_spec(&st, &r.a.spec).is_ok())
            };
            if ready {
                match self.upgrade(&Addr::root(), *id, true).await {
                    Ok(s) => tracing::info!(resident = %name, agent = %s.id, ty = %s.ty, "resident moved to its type's new version"),
                    Err(e) => tracing::warn!(resident = %name, error = %e, "upgrading resident failed"),
                }
            }
        }
        let by = c.version.as_ref().map_or_else(Addr::root, |v| v.applied_by.parse().unwrap_or_else(|_| Addr::root()));
        // A resident whose agent was cancelled (not moved by an upgrade) is
        // created afresh: cancelling a resident restarts it.
        let dead: Vec<String> = {
            let st = self.st.lock().await;
            existing
                .iter()
                .filter(|(_, id)| st.agents.get(id).is_none_or(|r| matches!(r.a.phase, subnet_core::agent::Phase::Cancelled) && r.a.superseded_by.is_none()))
                .map(|(n, _)| n.clone())
                .collect()
        };
        for (name, r) in &c.spec.residents {
            if existing.iter().any(|(n, _)| n == name) && !dead.contains(name) {
                continue;
            }
            match self.spawn(&by, &r.mixture, r.prompt.clone()).await {
                Ok(s) => {
                    tracing::info!(resident = %name, agent = %s.id, "resident created");
                    if let Err(e) = self.db.set_resident(name, s.id).await {
                        tracing::error!(error = %e, "storing resident failed");
                    }
                }
                // Typically its nodes aren't up yet; retried when a node gets ready.
                Err(e) => tracing::debug!(resident = %name, error = %e, "resident not created yet"),
            }
        }
    }

    /// Mailbox ops: agents may only use the mailboxes their mixture lists.
    async fn mailbox(&self, caller: &Addr, name: &str, max: u32, take: bool) -> Result<Vec<Mail>, HubError> {
        if let Addr::Agent(me) = caller {
            let mixture = self.st.lock().await.agents.get(me).and_then(|r| r.a.spec.mixture.clone());
            let allowed = mixture
                .and_then(|m| self.cluster.read().unwrap().spec.mixtures.get(&m).map(|x| x.mailboxes.contains(&name.to_string())))
                .unwrap_or(false);
            if !allowed {
                return Err(HubError::Forbidden(format!("mailbox {name:?} is not listed in this agent's mixture")));
            }
        }
        let addr = Addr::Mailbox(name.to_string());
        Ok(if take { self.db.take_mail_n(&addr, max).await? } else { self.db.peek_mail(&addr, max).await? })
    }
}

/// Constant-time comparison, so response timing doesn't leak the token.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
#[test]
fn ct_eq_works() {
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"ab"));
    assert!(ct_eq(b"", b""));
}

enum Work {
    Commit(AgentId, Vec<Event>),
    Place(AgentId),
}

/// Takes an agent off its node.
fn unassign(st: &mut State, id: AgentId) {
    if let Some(c) = st.agents.get_mut(&id).and_then(|r| r.node.take())
        && let Some(s) = st.nodes.get_mut(&c)
    {
        s.agents.remove(&id);
        let _ = s.tx.send(ToNode::Revoke { agent: id });
    }
}

/// Does this agent need a node right now? Idle, paused, failed or waiting
/// (on children or approval) agents don't: an event that gives them work
/// places them again.
fn wants_runner(a: &Agent) -> bool {
    if a.inflight() {
        return true;
    }
    let open = match a.pause {
        None => true,
        Some(PauseMode::Safe) => a.phase != Phase::Idle,
        Some(_) => false,
    };
    open && match &a.phase {
        Phase::Idle => !a.inbox.is_empty() || a.children.values().any(|r| !r.is_empty()),
        Phase::Thinking { .. } => true,
        Phase::Tools { calls } => {
            calls.iter().any(|c| matches!(c.state, CallState::Queued { .. } | CallState::Approved))
        }
        Phase::Failed { .. } | Phase::Cancelled => false,
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

/// Whether an agent runs an older version of its type or of its MCP servers
/// than the cluster declares now (`upgrade` moves it onto the current one).
fn outdated(c: &subnet_cluster::Cluster, spec: &Spec) -> bool {
    let name = spec.ty.split('@').next().unwrap_or_default();
    c.agent_id(name).is_some_and(|now| now != spec.ty) || spec.mcp.iter().any(|(m, id)| c.mcp_id(m).is_some_and(|now| &now != id))
}

fn summary(st: &State, c: &subnet_cluster::Cluster, id: AgentId, r: &AgentRec) -> AgentSummary {
    let awaiting_approval = r.a.awaiting_approval().first().map(|c| (*c).clone());
    let phase = serde_json::to_value(&r.a.phase).unwrap()["phase"].as_str().unwrap_or_default().to_string();
    AgentSummary {
        id,
        ty: r.a.spec.ty.clone(),
        parent: r.a.spec.parent,
        phase,
        pause: r.a.pause,
        paused: r.a.is_paused(),
        node: r.node.and_then(|c| st.nodes.get(&c)).map(|s| s.name.clone()),
        usage: r.a.usage,
        budget: r.a.spec.budget.clone(),
        reserved: r.a.reserved,
        compactions: r.a.compactions,
        tenant: r.a.spec.tenant.clone(),
        outdated: outdated(c, &r.a.spec),
        superseded_by: r.a.superseded_by,
        seq: r.seq,
        awaiting_approval,
        last: r
            .a
            .messages
            .iter()
            .rev()
            .find(|m| m.role == subnet_core::chat::Role::Assistant && m.content.as_deref().is_some_and(|c| !c.is_empty()))
            .and_then(|m| m.content.as_deref())
            .map(|c| c.chars().take(200).collect()),
    }
}

fn list_types(st: &State, c: &subnet_cluster::Cluster) -> Vec<TypeSummary> {
    let entry = |name: &str, kind: &str, agent: &str, mcp: Vec<String>, description: &str| {
        let id = c.agent_id(agent).unwrap_or_default();
        let offering: Vec<&NodeRec> = st.nodes.values().filter(|n| n.ready.contains(&id)).collect();
        TypeSummary {
            name: name.to_string(),
            kind: kind.to_string(),
            id,
            description: description.to_string(),
            mcp,
            nodes: offering.len() as u32,
            free: offering.iter().map(|n| n.capacity.saturating_sub(n.agents.len() as u32)).sum(),
        }
    };
    let mut v: Vec<_> = c
        .mixtures
        .iter()
        .map(|(n, m)| entry(n, "mixture", &m.agent, m.mcp.clone(), &m.description))
        .chain(c.agents.iter().map(|(n, a)| entry(n, "agent", n, vec![], &a.description)))
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}
