//! The spawner: runs the agents the hub assigns to it. Each agent is a task
//! holding a replica of its state machine; it applies committed events from
//! the hub and performs the resulting effects (LLM calls, tool calls).

pub mod config;
pub mod mcp;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use subnet_core::addr::AgentId;
use subnet_core::agent::{Agent, Effect, Event, Spec};
use subnet_core::chat::{Delta, Message, ToolCall, ToolDef};
use subnet_core::proto::{Op, ToHub, ToSpawner};
use subnet_core::tools::{builtin_op, builtin_tools};
use subnet_llm::Client;

use config::{Config, TypeConfig};

/// How often streamed deltas are flushed to the hub.
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// A type this spawner offers, with its live clients.
pub struct TypeRt {
    pub cfg: TypeConfig,
    pub llm: Client,
    pub mcp: mcp::McpTools,
}

impl TypeRt {
    pub async fn new(cfg: TypeConfig) -> anyhow::Result<Self> {
        let llm = Client::from_env(cfg.model.clone())
            .map_err(|e| anyhow::anyhow!("type {}: api key env {:?}: {e}", cfg.name, cfg.model.api_key_env))?;
        let builtins: Vec<_> = builtin_tools().into_iter().map(|t| t.name).collect();
        let mcp = mcp::McpTools::connect(&cfg.mcp, &builtins)
            .await
            .map_err(|e| anyhow::anyhow!("type {}: {e}", cfg.name))?;
        Ok(Self { cfg, llm, mcp })
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        let mut t = builtin_tools();
        t.extend(self.mcp.defs());
        t
    }

    fn idempotent(&self, name: &str) -> bool {
        self.cfg.idempotent.iter().any(|t| t == name)
    }
}

type Pending = std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>;

/// Shared by all agents of one hub connection.
struct Link {
    out: mpsc::UnboundedSender<ToHub>,
    next: AtomicU64,
    pending: Pending,
}

impl Link {
    async fn request(&self, agent: AgentId, epoch: u64, op: Op) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.out.send(ToHub::Request { id, agent, epoch, op }).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("hub connection lost".into());
        }
        rx.await.unwrap_or_else(|_| Err("hub connection lost".into()))
    }
}

pub struct Spawner {
    pub name: String,
    pub capacity: u32,
    pub token: Option<String>,
    types: HashMap<String, Arc<TypeRt>>,
}

impl Spawner {
    pub async fn new(cfg: &Config) -> anyhow::Result<Self> {
        let token = cfg.token_env.as_deref().map(std::env::var).transpose()?;
        let mut types = HashMap::new();
        for t in &cfg.types {
            let rt = TypeRt::new(t.clone()).await?;
            types.insert(t.info().id(), Arc::new(rt));
        }
        Ok(Self { name: cfg.name.clone(), capacity: cfg.capacity, token, types })
    }

    pub fn hello(&self) -> ToHub {
        let mut types: Vec<_> = self.types.values().map(|t| t.cfg.info()).collect();
        types.sort_by(|a, b| a.name.cmp(&b.name));
        ToHub::Hello { name: self.name.clone(), token: self.token.clone(), types, capacity: self.capacity }
    }

    /// Serves one hub connection until `inbox` closes. Every agent it ran is
    /// dropped at the end: the hub reassigns them.
    pub async fn serve(&self, mut inbox: mpsc::UnboundedReceiver<ToSpawner>, out: mpsc::UnboundedSender<ToHub>) {
        let link = Arc::new(Link { out, next: AtomicU64::new(1), pending: Default::default() });
        // Every agent dies with this connection, even if this future is dropped.
        let root = CancellationToken::new();
        let _guard = root.clone().drop_guard();
        let mut agents: HashMap<AgentId, (mpsc::UnboundedSender<(u64, Event)>, CancellationToken)> = HashMap::new();
        while let Some(msg) = inbox.recv().await {
            match msg {
                ToSpawner::Welcome => tracing::info!(name = %self.name, "connected to hub"),
                ToSpawner::Rejected { reason } => {
                    tracing::error!(%reason, "hub rejected us");
                    break;
                }
                ToSpawner::Assign { agent, epoch, spec, events } => {
                    let Some(rt) = self.types.get(&spec.ty).cloned() else {
                        tracing::error!(%agent, ty = %spec.ty, "assigned an agent of a type we don't offer");
                        continue;
                    };
                    if let Some((_, life)) = agents.remove(&agent) {
                        life.cancel();
                    }
                    let (tx, rx) = mpsc::unbounded_channel();
                    let life = root.child_token();
                    let runner = Runner::new(agent, epoch, spec, &events, rt, link.clone(), life.clone());
                    tokio::spawn(runner.run(rx));
                    agents.insert(agent, (tx, life));
                }
                ToSpawner::Commit { agent, seq, event } => {
                    if let Some((tx, _)) = agents.get(&agent) {
                        let _ = tx.send((seq, event));
                    }
                }
                ToSpawner::Revoke { agent } => {
                    if let Some((_, life)) = agents.remove(&agent) {
                        tracing::info!(%agent, "revoked");
                        life.cancel();
                    }
                }
                ToSpawner::Reply { id, result } => {
                    if let Some(tx) = link.pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
            }
        }
    }
}

/// One agent on this spawner.
struct Runner {
    id: AgentId,
    epoch: u64,
    a: Agent,
    seq: u64,
    rt: Arc<TypeRt>,
    link: Arc<Link>,
    /// Cancelled on revoke: stop everything and write nothing more.
    life: CancellationToken,
    /// Cancelled by `AbortInflight`: stop and record what was produced.
    inflight: CancellationToken,
    /// Effects from replay are held until the log is caught up.
    startup: Vec<Effect>,
}

impl Runner {
    fn new(id: AgentId, epoch: u64, spec: Spec, events: &[Event], rt: Arc<TypeRt>, link: Arc<Link>, life: CancellationToken) -> Self {
        // The hub ends every assignment's log with `Recovered`; its effects
        // are where this runner starts.
        let (last, prefix) = events.split_last().expect("assignment without events");
        if *last != Event::Recovered {
            tracing::error!(agent = %id, ?last, "assignment does not end with Recovered");
        }
        let mut a = Agent::replay(id, spec, prefix);
        let startup = a.apply(last);
        let inflight = life.child_token();
        Self { id, epoch, a, seq: events.len() as u64, rt, link, life, inflight, startup }
    }

    async fn run(mut self, mut commits: mpsc::UnboundedReceiver<(u64, Event)>) {
        tracing::info!(agent = %self.id, epoch = self.epoch, seq = self.seq, phase = ?self.a.phase, "running");
        let fx = std::mem::take(&mut self.startup);
        self.exec(fx);
        loop {
            tokio::select! {
                _ = self.life.cancelled() => break,
                c = commits.recv() => {
                    let Some((seq, event)) = c else { break };
                    if seq <= self.seq {
                        continue; // already in the replayed log
                    }
                    if seq != self.seq + 1 {
                        tracing::error!(agent = %self.id, expected = self.seq + 1, got = seq, "gap in commits, giving the agent up");
                        break;
                    }
                    self.seq = seq;
                    let fx = self.a.apply(&event);
                    self.exec(fx);
                }
            }
        }
        self.life.cancel();
    }

    fn proposer(&self) -> Proposer {
        Proposer { agent: self.id, epoch: self.epoch, out: self.link.out.clone(), life: self.life.clone() }
    }

    fn exec(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::CallLlm => {
                    self.inflight = self.life.child_token();
                    let mut msgs = vec![Message::system(self.rt.cfg.system.clone())];
                    msgs.extend(self.a.llm_messages());
                    tokio::spawn(llm_call(self.rt.clone(), msgs, self.proposer(), self.inflight.clone()));
                }
                Effect::CallTool { call, retry } => {
                    self.inflight = self.life.child_token();
                    let t = ToolTask {
                        rt: self.rt.clone(),
                        link: self.link.clone(),
                        agent: self.id,
                        epoch: self.epoch,
                        p: self.proposer(),
                        abort: self.inflight.clone(),
                    };
                    tokio::spawn(t.run(call, retry));
                }
                Effect::AbortInflight => self.inflight.cancel(),
                Effect::RequestApproval { call } => {
                    tracing::info!(agent = %self.id, tool = %call.function.name, "waiting for approval");
                }
                // The hub routes reports from its own replica.
                Effect::Report { .. } => {}
            }
        }
    }
}

#[derive(Clone)]
struct Proposer {
    agent: AgentId,
    epoch: u64,
    out: mpsc::UnboundedSender<ToHub>,
    life: CancellationToken,
}

impl Proposer {
    fn propose(&self, events: Vec<Event>) {
        if self.life.is_cancelled() || events.is_empty() {
            return;
        }
        let _ = self.out.send(ToHub::Propose { agent: self.agent, epoch: self.epoch, events });
    }
}

fn merge(buf: &mut Delta, d: Delta) {
    if let Some(c) = d.content {
        buf.content.get_or_insert_default().push_str(&c);
    }
    buf.tool_calls.extend(d.tool_calls);
    if d.finish_reason.is_some() {
        buf.finish_reason = d.finish_reason;
    }
    if d.usage.is_some() {
        buf.usage = d.usage;
    }
}

fn take_delta(buf: &mut Delta) -> Option<Event> {
    (*buf != Delta::default()).then(|| Event::LlmDelta { delta: std::mem::take(buf) })
}

async fn llm_call(rt: Arc<TypeRt>, msgs: Vec<Message>, p: Proposer, abort: CancellationToken) {
    let tools = rt.tool_defs();
    let stream = tokio::select! {
        s = rt.llm.stream(&msgs, &tools) => s,
        _ = abort.cancelled() => return p.propose(vec![Event::LlmAborted]),
    };
    let mut s = match stream {
        Ok(s) => s,
        Err(e) => return p.propose(vec![Event::LlmFailed { error: e.to_string() }]),
    };
    let mut buf = Delta::default();
    let mut flush = tokio::time::interval(FLUSH_EVERY);
    flush.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = abort.cancelled() => {
                let mut evs: Vec<_> = take_delta(&mut buf).into_iter().collect();
                evs.push(Event::LlmAborted);
                return p.propose(evs);
            }
            d = s.next() => match d {
                Some(Ok(d)) => merge(&mut buf, d),
                Some(Err(e)) => {
                    let mut evs: Vec<_> = take_delta(&mut buf).into_iter().collect();
                    evs.push(Event::LlmFailed { error: e.to_string() });
                    return p.propose(evs);
                }
                None => {
                    let mut evs: Vec<_> = take_delta(&mut buf).into_iter().collect();
                    evs.push(Event::LlmDone);
                    return p.propose(evs);
                }
            },
            _ = flush.tick() => p.propose(take_delta(&mut buf).into_iter().collect()),
        }
    }
}

struct ToolTask {
    rt: Arc<TypeRt>,
    link: Arc<Link>,
    agent: AgentId,
    epoch: u64,
    p: Proposer,
    abort: CancellationToken,
}

impl ToolTask {
    async fn run(self, call: ToolCall, retry: bool) {
        let aborted = || Event::ToolAborted { call_id: call.id.clone() };
        let done = |r: Result<String, String>| match r {
            Ok(content) => Event::ToolResult { call_id: call.id.clone(), content, is_error: false },
            Err(e) => Event::ToolResult { call_id: call.id.clone(), content: format!("error: {e}"), is_error: true },
        };
        let name = call.function.name.as_str();
        let r = match builtin_op(&call) {
            Some(Err(e)) => Some(Err(e)),
            // Read-only ops are safe to repeat; the rest may have happened already.
            Some(Ok(op)) if retry && !matches!(op, Op::ListAgents | Op::ListTypes) => None,
            Some(Ok(op)) => tokio::select! {
                r = self.link.request(self.agent, self.epoch, op) => Some(r.map(|v| v.to_string())),
                _ = self.abort.cancelled() => None,
            },
            None if retry && self.rt.mcp.has(name) && !self.rt.idempotent(name) => None,
            None => self.rt.mcp.call(name, &call.function.arguments, &self.abort).await,
        };
        let ev = r.map_or_else(aborted, done);
        self.p.propose(vec![ev]);
    }
}

/// Runs a spawner against an in-process hub (single-process mode and tests).
pub async fn attach(hub: Arc<crate::hub::Hub>, spawner: Arc<Spawner>) -> anyhow::Result<crate::hub::ConnId> {
    let mut hello = spawner.hello();
    if let ToHub::Hello { token, .. } = &mut hello {
        *token = hub.token();
    }
    let (conn, inbox) = hub.connect(hello).await.map_err(anyhow::Error::msg)?;
    let (out, mut out_rx) = mpsc::unbounded_channel();
    let h = hub.clone();
    tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if let Err(e) = h.handle(conn, m).await {
                tracing::warn!(error = %e, "local spawner message failed");
            }
        }
    });
    tokio::spawn(async move {
        spawner.serve(inbox, out).await;
        hub.disconnect(conn).await;
    });
    Ok(conn)
}
