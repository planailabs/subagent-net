//! A node: runs what the hub's cluster spec assigns to it (agent types, MCP
//! servers). Each assigned agent is a task holding a replica of its state
//! machine; it applies committed events from the hub and performs the
//! resulting effects (LLM calls, tool calls).

pub mod external;
pub mod mcp;
pub mod relay;
pub mod senses;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use subnet_cluster::{NodeAgent, NodeConfig};
use subnet_core::addr::AgentId;
use subnet_core::agent::{Agent, Effect, Event, Spec};
use subnet_core::chat::{Delta, Message, ToolCall};
use subnet_core::proto::Op;
use subnet_core::tools::builtin_op;
use subnet_llm::{Client, ModelConfig};

use crate::wire::{AgentStatus, McpStatus, Snapshot, ToHub, ToNode};
use mcp::McpHost;

/// How often streamed deltas are flushed to the hub.
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// What produces an agent's assistant messages.
pub enum Brain {
    Llm(Client),
    External(Arc<external::External>),
}

/// An agent type this node runs.
pub struct AgentRt {
    pub id: String,
    pub system: String,
    pub brain: Brain,
}

impl AgentRt {
    fn new(a: &NodeAgent) -> Result<Self, String> {
        if a.def.executor.is_external() {
            let ext = external::External::new(a)?;
            return Ok(Self {
                id: a.id.clone(),
                system: a.def.system_prompt.clone(),
                brain: Brain::External(Arc::new(ext)),
            });
        }
        let key = match &a.def.credential.env {
            Some(var) => Some(std::env::var(var).map_err(|e| format!("credential env {var}: {e}"))?),
            None => None,
        };
        let cfg = ModelConfig {
            base_url: a.def.credential.base_url.clone(),
            model: a.def.model.clone(),
            api_key_env: None,
            prefill: a.def.prefill,
            params: a.def.params.clone(),
        };
        Ok(Self { id: a.id.clone(), system: a.def.system_prompt.clone(), brain: Brain::Llm(Client::new(cfg, key)) })
    }

    /// Streams the next assistant message.
    async fn think(
        &self,
        agent: AgentId,
        msgs: &[Message],
        tools: &[subnet_core::chat::ToolDef],
    ) -> Result<futures::stream::BoxStream<'static, Result<Delta, String>>, String> {
        match &self.brain {
            Brain::Llm(c) => {
                let s = c.stream(msgs, tools).await.map_err(|e| e.to_string())?;
                Ok(Box::pin(s.map(|r| r.map_err(|e| e.to_string()))))
            }
            // The external process gets the system prompt separately.
            Brain::External(x) => x.think(agent, &self.system, &msgs[1..], tools).await,
        }
    }
}

/// What this node currently runs; survives reconnects so MCP servers stay up.
#[derive(Default)]
struct Rt {
    agents: HashMap<String, Arc<AgentRt>>,
    mcps: HashMap<String, Arc<McpHost>>,
    errors: HashMap<String, String>,
}

/// The running stream relay: its link (key), streams out and in, and stop token.
type RunningRelay = (String, Vec<String>, Vec<String>, CancellationToken);

pub struct Node {
    pub name: String,
    pub token: Option<String>,
    rt: RwLock<Rt>,
    pub senses: Arc<senses::Senses>,
    /// Sense output, forwarded to whichever hub connection is up.
    sense_rx: tokio::sync::Mutex<mpsc::Receiver<senses::SenseOut>>,
    /// How to reach the hub's stream relay (set by whoever connects us).
    relay_link: std::sync::Mutex<Option<relay::RelayLink>>,
    relay: std::sync::Mutex<Option<RunningRelay>>,
}

type Pending<T> = std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<T, String>>>>;

/// Shared by all agents of one hub connection.
struct Link {
    out: mpsc::UnboundedSender<ToHub>,
    next: AtomicU64,
    requests: Pending<Value>,
    mcp_calls: Pending<String>,
}

impl Link {
    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    async fn request(&self, agent: AgentId, epoch: u64, op: Op) -> Result<Value, String> {
        let id = self.id();
        let (tx, rx) = oneshot::channel();
        self.requests.lock().unwrap().insert(id, tx);
        if self.out.send(ToHub::Request { id, agent, epoch, op }).is_err() {
            self.requests.lock().unwrap().remove(&id);
            return Err("hub connection lost".into());
        }
        rx.await.unwrap_or_else(|_| Err("hub connection lost".into()))
    }

    /// A tool of an MCP type hosted elsewhere, via the hub. `None` = aborted.
    async fn mcp_call(
        &self,
        agent: AgentId,
        epoch: u64,
        mcp: &str,
        tool: &str,
        args: Value,
        abort: &CancellationToken,
    ) -> Option<Result<String, String>> {
        let id = self.id();
        let (tx, rx) = oneshot::channel();
        self.mcp_calls.lock().unwrap().insert(id, tx);
        let msg = ToHub::McpCall { id, agent, epoch, mcp: mcp.into(), tool: tool.into(), args };
        if self.out.send(msg).is_err() {
            self.mcp_calls.lock().unwrap().remove(&id);
            return Some(Err("hub connection lost".into()));
        }
        tokio::select! {
            r = rx => Some(r.unwrap_or_else(|_| Err("hub connection lost".into()))),
            _ = abort.cancelled() => {
                self.mcp_calls.lock().unwrap().remove(&id);
                let _ = self.out.send(ToHub::McpCancel { id });
                None
            }
        }
    }
}

impl Node {
    pub fn new(name: &str, token: Option<String>) -> Self {
        // ponytail: bounded; events produced while the hub is unreachable beyond this are dropped (and logged).
        let (tx, rx) = mpsc::channel(1024);
        Self {
            name: name.to_string(),
            token,
            rt: RwLock::new(Rt::default()),
            senses: senses::Senses::new(tx),
            sense_rx: tokio::sync::Mutex::new(rx),
            relay_link: Default::default(),
            relay: Default::default(),
        }
    }

    pub fn set_relay_link(&self, link: relay::RelayLink) {
        *self.relay_link.lock().unwrap() = Some(link);
    }

    /// (Re)starts the stream relay if its streams or its hub changed.
    fn configure_relay(&self, out: &[String], inn: &[String]) {
        let link = self.relay_link.lock().unwrap().clone();
        let key = link.as_ref().map(relay::RelayLink::key).unwrap_or_default();
        let mut cur = self.relay.lock().unwrap();
        if cur.as_ref().is_some_and(|(k, o, i, _)| *k == key && o == out && i == inn) {
            return;
        }
        if let Some((_, _, _, stop)) = cur.take() {
            stop.cancel();
        }
        if out.is_empty() && inn.is_empty() {
            return;
        }
        let Some(link) = link else {
            tracing::warn!("streams need relaying but this node has no relay link");
            return;
        };
        let stop = CancellationToken::new();
        tokio::spawn(relay::run(link, self.senses.clone(), out.to_vec(), inn.to_vec(), stop.clone()));
        *cur = Some((key, out.to_vec(), inn.to_vec(), stop));
    }

    pub fn hello(&self) -> ToHub {
        ToHub::Hello { name: self.name.clone(), token: self.token.clone() }
    }

    /// Starts what `cfg` asks for, stops what it no longer does, and reports
    /// what is available. Runtimes of unchanged types are kept.
    pub async fn configure(&self, cfg: &NodeConfig) -> ToHub {
        self.senses.configure(&cfg.senses);
        self.configure_relay(&cfg.relay_out, &cfg.relay_in);
        let mut rt = self.rt.write().await;
        rt.agents.retain(|id, _| cfg.agents.iter().any(|a| &a.id == id));
        rt.mcps.retain(|id, _| cfg.mcps.iter().any(|m| &m.id == id));
        rt.errors.clear();
        for a in &cfg.agents {
            if rt.agents.contains_key(&a.id) {
                continue;
            }
            match AgentRt::new(a) {
                Ok(r) => {
                    rt.agents.insert(a.id.clone(), Arc::new(r));
                }
                Err(e) => {
                    tracing::warn!(agent = %a.id, error = %e, "agent type unavailable");
                    rt.errors.insert(a.id.clone(), e);
                }
            }
        }
        for m in &cfg.mcps {
            if rt.mcps.contains_key(&m.id) {
                continue;
            }
            match McpHost::connect(&m.name, &m.id, &m.def).await {
                Ok(h) => {
                    rt.mcps.insert(m.id.clone(), Arc::new(h));
                }
                Err(e) => {
                    tracing::warn!(mcp = %m.id, error = %e, "mcp server unavailable");
                    rt.errors.insert(m.id.clone(), e);
                }
            }
        }
        let agents = cfg
            .agents
            .iter()
            .map(|a| AgentStatus { id: a.id.clone(), error: rt.errors.get(&a.id).cloned() })
            .collect();
        let mcps = cfg
            .mcps
            .iter()
            .map(|m| McpStatus {
                id: m.id.clone(),
                tools: rt.mcps.get(&m.id).map(|h| h.defs()).unwrap_or_default(),
                error: rt.errors.get(&m.id).cloned(),
            })
            .collect();
        ToHub::Ready { agents, mcps }
    }

    /// Serves one hub connection until `inbox` closes. Every agent it ran is
    /// dropped at the end: the hub reassigns them.
    pub async fn serve(&self, mut inbox: mpsc::UnboundedReceiver<ToNode>, out: mpsc::UnboundedSender<ToHub>) {
        let link = Arc::new(Link {
            out: out.clone(),
            next: AtomicU64::new(1),
            requests: Default::default(),
            mcp_calls: Default::default(),
        });
        // Every agent and invocation dies with this connection, even if this future is dropped.
        let root = CancellationToken::new();
        let _guard = root.clone().drop_guard();
        let mut agents: HashMap<AgentId, (mpsc::UnboundedSender<(u64, Event)>, CancellationToken)> = HashMap::new();
        let mut invocations: HashMap<u64, CancellationToken> = HashMap::new();
        let mut sense_rx = self.sense_rx.lock().await;
        loop {
            let msg = tokio::select! {
                m = inbox.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                s = sense_rx.recv() => {
                    let msg = match s {
                        Some(senses::SenseOut::Event { sense, mut data }) => {
                            for (hash, mime, base64) in senses::extract_blobs(&mut data) {
                                let _ = out.send(ToHub::Blob { hash, mime, base64 });
                            }
                            ToHub::SenseEvent {
                                sense,
                                id: uuid::Uuid::new_v4().to_string(),
                                at: chrono::Utc::now().timestamp_millis() as u64,
                                data,
                            }
                        }
                        Some(senses::SenseOut::Status { sense, error }) => ToHub::SenseStatus { sense, error },
                        None => continue,
                    };
                    let _ = out.send(msg);
                    continue;
                }
            };
            match msg {
                ToNode::Welcome => tracing::info!(name = %self.name, "connected to hub"),
                ToNode::Rejected { reason } => {
                    tracing::error!(%reason, "hub rejected us");
                    break;
                }
                ToNode::Configure { config } => {
                    let ready = self.configure(&config).await;
                    let _ = out.send(ready);
                }
                ToNode::Assign { agent, epoch, spec, snapshot, events } => {
                    let Some(rt) = self.rt.read().await.agents.get(&spec.ty).cloned() else {
                        tracing::error!(%agent, ty = %spec.ty, "assigned an agent of a type this node doesn't run");
                        continue;
                    };
                    if let Some((_, life)) = agents.remove(&agent) {
                        life.cancel();
                    }
                    let (tx, rx) = mpsc::unbounded_channel();
                    let life = root.child_token();
                    let mcps = self.rt.read().await.mcps.clone();
                    let runner = Runner::new(agent, epoch, spec, snapshot, &events, rt, mcps, link.clone(), life.clone());
                    tokio::spawn(runner.run(rx));
                    agents.insert(agent, (tx, life));
                }
                ToNode::Commit { agent, seq, event } => {
                    if let Some((tx, _)) = agents.get(&agent) {
                        let _ = tx.send((seq, event));
                    }
                }
                ToNode::Revoke { agent } => {
                    if let Some((_, life)) = agents.remove(&agent) {
                        tracing::info!(%agent, "revoked");
                        life.cancel();
                    }
                }
                ToNode::Reply { id, result } => {
                    if let Some(tx) = link.requests.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
                ToNode::McpReply { id, result } => {
                    if let Some(tx) = link.mcp_calls.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                }
                ToNode::McpInvoke { id, mcp, tool, args } => {
                    invocations.retain(|_, t| !t.is_cancelled());
                    let abort = root.child_token();
                    invocations.insert(id, abort.clone());
                    let host = self.rt.read().await.mcps.get(&mcp).cloned();
                    let out = out.clone();
                    tokio::spawn(async move {
                        let result = match host {
                            None => Err(format!("this node doesn't run mcp {mcp}")),
                            Some(h) => h.call(&tool, args, &abort).await.unwrap_or_else(|| Err("aborted".into())),
                        };
                        abort.cancel(); // marks the invocation finished
                        let _ = out.send(ToHub::McpResult { id, result });
                    });
                }
                ToNode::McpAbort { id } => {
                    if let Some(t) = invocations.remove(&id) {
                        t.cancel();
                    }
                }
            }
        }
    }
}

/// One agent on this node.
struct Runner {
    id: AgentId,
    epoch: u64,
    a: Agent,
    seq: u64,
    rt: Arc<AgentRt>,
    mcps: HashMap<String, Arc<McpHost>>,
    link: Arc<Link>,
    /// Cancelled on revoke: stop everything and write nothing more.
    life: CancellationToken,
    /// Parent of every in-flight call's token; `AbortInflight` cancels it
    /// (the calls record what they produced) and starts a new one.
    inflight: CancellationToken,
    /// Effects from replay are held until the log is caught up.
    startup: Vec<Effect>,
}

impl Runner {
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: AgentId,
        epoch: u64,
        spec: Spec,
        snapshot: Option<Box<Snapshot>>,
        events: &[Event],
        rt: Arc<AgentRt>,
        mcps: HashMap<String, Arc<McpHost>>,
        link: Arc<Link>,
        life: CancellationToken,
    ) -> Self {
        // The hub ends every assignment's log with `Recovered`; its effects
        // are where this runner starts.
        let (last, prefix) = events.split_last().expect("assignment without events");
        if *last != Event::Recovered {
            tracing::error!(agent = %id, ?last, "assignment does not end with Recovered");
        }
        let (base, start) = match snapshot {
            Some(s) => (s.seq, s.state),
            None => (0, Agent::new(id, spec)),
        };
        let mut a = start.fold(prefix);
        let startup = a.apply(last);
        let inflight = life.child_token();
        Self { id, epoch, a, seq: base + events.len() as u64, rt, mcps, link, life, inflight, startup }
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
                    let mut msgs = vec![Message::system(self.rt.system.clone())];
                    msgs.extend(self.a.llm_messages());
                    let tools = self.a.offered_tools();
                    let abort = self.inflight.child_token();
                    tokio::spawn(llm_call(self.rt.clone(), self.id, msgs, tools, self.proposer(), abort));
                }
                Effect::Compact { upto } => {
                    let msgs = self.a.compaction_request(&self.rt.system, upto);
                    let abort = self.inflight.child_token();
                    tokio::spawn(compact_call(self.rt.clone(), upto, msgs, self.proposer(), abort));
                }
                Effect::CallTool { call, retry } => {
                    let t = ToolTask {
                        spec: self.a.spec.clone(),
                        mcps: self.mcps.clone(),
                        link: self.link.clone(),
                        agent: self.id,
                        epoch: self.epoch,
                        p: self.proposer(),
                        abort: self.inflight.child_token(),
                    };
                    tokio::spawn(t.run(call, retry));
                }
                // Aborts every running call of this agent (tools run in parallel).
                Effect::AbortInflight => {
                    self.inflight.cancel();
                    self.inflight = self.life.child_token();
                }
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

async fn llm_call(
    rt: Arc<AgentRt>,
    agent: AgentId,
    msgs: Vec<Message>,
    tools: Vec<subnet_core::chat::ToolDef>,
    p: Proposer,
    abort: CancellationToken,
) {
    let stream = tokio::select! {
        s = rt.think(agent, &msgs, &tools) => s,
        _ = abort.cancelled() => return p.propose(vec![Event::LlmAborted]),
    };
    let mut s = match stream {
        Ok(s) => s,
        Err(error) => return p.propose(vec![Event::LlmFailed { error }]),
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
                Some(Err(error)) => {
                    let mut evs: Vec<_> = take_delta(&mut buf).into_iter().collect();
                    evs.push(Event::LlmFailed { error });
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

/// Writes a compaction's summary: one model call without tools. A failure
/// is reported (`CompactFailed`) and the agent goes on uncompacted.
async fn compact_call(rt: Arc<AgentRt>, upto: usize, msgs: Vec<Message>, p: Proposer, abort: CancellationToken) {
    let Brain::Llm(c) = &rt.brain else {
        return p.propose(vec![Event::CompactFailed { error: "external executors don't compact".into() }]);
    };
    let summarise = async {
        let mut s = c.stream(&msgs, &[]).await.map_err(|e| e.to_string())?;
        let (mut text, mut usage) = (String::new(), None);
        while let Some(d) = s.next().await {
            let d = d.map_err(|e| e.to_string())?;
            text.push_str(d.content.as_deref().unwrap_or_default());
            usage = d.usage.or(usage);
        }
        Ok::<_, String>((text, usage))
    };
    let ev = tokio::select! {
        r = summarise => match r {
            Ok((summary, usage)) if !summary.trim().is_empty() => Event::Compacted { upto, summary, usage },
            Ok(_) => Event::CompactFailed { error: "the summary came back empty".into() },
            Err(error) => Event::CompactFailed { error },
        },
        _ = abort.cancelled() => Event::LlmAborted,
    };
    if let Event::CompactFailed { error } = &ev {
        tracing::warn!(agent = %p.agent, %error, "compaction failed; going on without");
    }
    p.propose(vec![ev]);
}

struct ToolTask {
    spec: Spec,
    mcps: HashMap<String, Arc<McpHost>>,
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
            Some(Ok(op)) if retry && !matches!(op, Op::ListAgents | Op::ListTypes | Op::MailboxPeek { .. }) => None,
            Some(Ok(op)) => tokio::select! {
                r = self.link.request(self.agent, self.epoch, op) => Some(r.map(|v| v.to_string())),
                _ = self.abort.cancelled() => None,
            },
            None if retry && !self.spec.idempotent.iter().any(|t| t == name) => None,
            None => self.mcp(name, &call.function.arguments).await,
        };
        self.p.propose(vec![r.map_or_else(aborted, done)]);
    }

    /// `<mcp>.<tool>`: on this node if it runs that MCP type, else via the hub.
    async fn mcp(&self, name: &str, args: &str) -> Option<Result<String, String>> {
        let Some((server, tool)) = name.split_once('.') else {
            return Some(Err(format!("unknown tool {name:?}")));
        };
        let Some(id) = self.spec.mcp.get(server) else {
            return Some(Err(format!("unknown tool {name:?}")));
        };
        let args = match mcp::parse_args(args) {
            Ok(a) => a,
            Err(e) => return Some(Err(e)),
        };
        match self.mcps.get(id) {
            Some(h) => h.call(tool, args, &self.abort).await,
            None => self.link.mcp_call(self.agent, self.epoch, id, tool, args, &self.abort).await,
        }
    }
}

/// Runs a node against an in-process hub (single-process mode and tests).
pub async fn attach(hub: Arc<crate::hub::Hub>, node: Arc<Node>) -> anyhow::Result<crate::hub::ConnId> {
    node.set_relay_link(relay::RelayLink::Local(hub.clone()));
    let mut hello = node.hello();
    if let ToHub::Hello { token, .. } = &mut hello {
        *token = hub.token();
    }
    let (conn, inbox) = hub.connect(hello).await.map_err(anyhow::Error::msg)?;
    let (out, mut out_rx) = mpsc::unbounded_channel();
    let h = hub.clone();
    tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if let Err(e) = h.handle(conn, m).await {
                tracing::warn!(error = %e, "local node message failed");
            }
        }
    });
    tokio::spawn(async move {
        node.serve(inbox, out).await;
        hub.disconnect(conn).await;
    });
    Ok(conn)
}
