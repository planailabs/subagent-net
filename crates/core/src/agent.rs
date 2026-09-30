//! The agent state machine. An agent is a fold over its committed event log:
//! `apply` consumes one event and returns the side effects to perform, and
//! `replay` + `recover` rebuild a live agent after a crash or a move to
//! another spawner. No I/O happens here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::addr::{Addr, AgentId};
use crate::chat::{Accumulator, Delta, Message, ToolCall, ToolDef, Usage};

/// Name of the one built-in tool the state machine handles itself.
pub const WAIT_FOR: &str = "wait_for";
/// Loads lazy tools' schemas; resolved by the state machine itself.
pub const LOAD_TOOLS: &str = "load_tools";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Budget {
    /// Total tokens (prompt + completion) this agent may spend. `None` = unlimited.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// How many levels of descendants this agent may create below itself.
    #[serde(default)]
    pub max_depth: u32,
    #[serde(default)]
    pub max_children: u32,
}

/// What an agent is, fixed when it is created (stored beside the log).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// `name@hash` of the agent type.
    pub ty: String,
    /// The mixture it was spawned as, if any.
    #[serde(default)]
    pub mixture: Option<String>,
    #[serde(default)]
    pub parent: Option<AgentId>,
    #[serde(default)]
    pub budget: Budget,
    /// Tool names that need user approval before running.
    #[serde(default)]
    pub approve: Vec<String>,
    /// MCP types it may use: name → `name@hash`.
    #[serde(default)]
    pub mcp: BTreeMap<String, String>,
    /// Tools offered to the model: built-ins and `<mcp>.<tool>`.
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    /// Tools safe to re-run after a crash (`<mcp>.<tool>`).
    #[serde(default)]
    pub idempotent: Vec<String>,
    /// Tools of `tools` that are lazy: the model only sees their names (in
    /// `load_tools`' description) until they're loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lazy: Vec<String>,
}

impl Spec {
    /// A spec with only a type (tests and bare agents).
    pub fn of_type(ty: impl Into<String>) -> Self {
        Self {
            ty: ty.into(),
            mixture: None,
            parent: None,
            budget: Budget::default(),
            approve: vec![],
            mcp: BTreeMap::new(),
            tools: vec![],
            idempotent: vec![],
            lazy: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PauseMode {
    /// Finish the current turn, then stop.
    Safe,
    /// Finish the in-flight LLM stream or tool call, then stop.
    Quick,
    /// Abort whatever is in flight now; partial output is kept.
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Failed,
    Cancelled,
}

/// A committed log entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// `reply` marks an automatic turn-end answer: it wakes the agent but its
    /// sender is not owed an answer back, so agents can't ping-pong forever.
    Inbox {
        from: Addr,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply: bool,
    },
    LlmDelta {
        delta: Delta,
    },
    LlmDone,
    LlmAborted,
    LlmFailed {
        error: String,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    ToolAborted {
        call_id: String,
    },
    Approval {
        call_id: String,
        approved: bool,
    },
    /// `reserved` tokens are carved out of this agent's budget for the child.
    ChildSpawned {
        id: AgentId,
        #[serde(default)]
        reserved: u64,
    },
    ChildReport {
        id: AgentId,
        status: Status,
        content: String,
    },
    PauseRequested {
        mode: PauseMode,
    },
    Resumed,
    Cancelled,
    /// The agent was (re)placed on a spawner: whatever was in flight before is
    /// gone. Logged so every replica folds the same state.
    Recovered,
    /// Lazy tools loaded ahead of a message (the hub's router).
    ToolsLoaded {
        names: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    /// Stream a completion for `Agent::llm_messages()`; propose `LlmDelta`s then
    /// `LlmDone`, `LlmAborted` or `LlmFailed`.
    CallLlm,
    /// Run a tool; propose `ToolResult`. `retry` is set when the call was started
    /// before a crash: the runtime re-runs idempotent tools and proposes
    /// `ToolAborted` for the rest.
    CallTool { call: ToolCall, retry: bool },
    /// Ask the user to approve a call; they answer with `Approval`.
    RequestApproval { call: ToolCall },
    /// Abort the in-flight LLM stream / tool call (hard pause, cancel).
    AbortInflight,
    /// The turn ended: deliver the final answer to everyone who asked. The hub
    /// performs this from its own replica.
    Report { to: Vec<Addr>, status: Status, content: String },
}

/// Where one tool call of the current assistant message stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CallState {
    /// Not started; `retry` if it was running when its node died.
    Queued {
        retry: bool,
    },
    Running,
    /// Waiting for the user to approve or deny it.
    Approval,
    /// Approved, not started yet (e.g. paused meanwhile).
    Approved,
    /// `wait_for`: waiting for these children to report.
    Children {
        ids: Vec<AgentId>,
    },
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub call: ToolCall,
    pub state: CallState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Thinking {
        running: bool,
    },
    /// Tool calls of the last assistant message, run in parallel.
    Tools {
        calls: Vec<PendingCall>,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Queued {
    pub from: Addr,
    pub content: String,
    pub reply: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub status: Status,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub spec: Spec,
    pub messages: Vec<Message>,
    /// The assistant message being streamed; kept across aborts as the partial.
    pub acc: Accumulator,
    pub phase: Phase,
    pub pause: Option<PauseMode>,
    /// Messages waiting for the next LLM call.
    pub inbox: VecDeque<Queued>,
    /// Who gets this turn's final answer.
    pub reply_to: BTreeSet<Addr>,
    /// Askers of the last turn that had any; a turn woken only by replies or
    /// child reports answers them.
    pub askers: BTreeSet<Addr>,
    /// Children and their reports not yet shown to the model.
    pub children: BTreeMap<AgentId, Vec<Report>>,
    pub usage: Usage,
    /// Tokens handed to children.
    pub reserved: u64,
    /// Lazy tools whose schemas the model has been given.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub loaded: BTreeSet<String>,
}

impl Agent {
    pub fn new(id: AgentId, spec: Spec) -> Self {
        Self {
            id,
            spec,
            messages: vec![],
            acc: Accumulator::default(),
            phase: Phase::Idle,
            pause: None,
            inbox: VecDeque::new(),
            reply_to: BTreeSet::new(),
            askers: BTreeSet::new(),
            children: BTreeMap::new(),
            usage: Usage::default(),
            reserved: 0,
            loaded: BTreeSet::new(),
        }
    }

    /// Folds a log without performing effects.
    pub fn replay<'a>(id: AgentId, spec: Spec, events: impl IntoIterator<Item = &'a Event>) -> Self {
        Self::new(id, spec).fold(events)
    }

    /// Continues folding from this state (e.g. a snapshot).
    pub fn fold<'a>(mut self, events: impl IntoIterator<Item = &'a Event>) -> Self {
        for e in events {
            self.apply(e);
        }
        self
    }

    /// Whatever was in flight is gone: mark it so and return the effects that
    /// continue from here. Applied via `Event::Recovered`.
    fn recover(&mut self) -> Vec<Effect> {
        match &mut self.phase {
            Phase::Thinking { running } => *running = false,
            Phase::Tools { calls } => {
                for c in calls.iter_mut().filter(|c| c.state == CallState::Running) {
                    c.state = CallState::Queued { retry: true };
                }
            }
            _ => {}
        }
        // Tool-call fragments of a dead stream can't be continued.
        self.acc.tool_calls.clear();
        let mut fx = vec![];
        self.rerequest_approvals(&mut fx);
        self.advance(&mut fx);
        fx
    }

    /// Asks again for approvals still open (after a restart or resume).
    fn rerequest_approvals(&self, fx: &mut Vec<Effect>) {
        if let (Phase::Tools { calls }, true) = (&self.phase, self.gate()) {
            for c in calls.iter().filter(|c| c.state == CallState::Approval) {
                fx.push(Effect::RequestApproval { call: c.call.clone() });
            }
        }
    }

    /// Tool calls waiting for approval.
    pub fn awaiting_approval(&self) -> Vec<&ToolCall> {
        match &self.phase {
            Phase::Tools { calls } => calls.iter().filter(|c| c.state == CallState::Approval).map(|c| &c.call).collect(),
            _ => vec![],
        }
    }

    /// Tokens left for this agent and future children; `None` = unlimited.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.spec.budget.max_tokens.map(|m| m.saturating_sub(self.usage.total() + self.reserved))
    }

    /// True when something is running on the spawner for this agent.
    pub fn inflight(&self) -> bool {
        match &self.phase {
            Phase::Thinking { running } => *running,
            Phase::Tools { calls } => calls.iter().any(|c| c.state == CallState::Running),
            _ => false,
        }
    }

    /// Paused and nothing left to finish.
    pub fn is_paused(&self) -> bool {
        match self.pause {
            None => false,
            Some(PauseMode::Safe) => self.phase == Phase::Idle,
            Some(_) => !self.inflight(),
        }
    }

    /// Transcript for the next LLM call (without the system prompt). A partial
    /// answer is the trailing assistant message to continue.
    pub fn llm_messages(&self) -> Vec<Message> {
        let mut m = self.messages.clone();
        if !self.acc.is_empty() {
            m.push(self.acc.partial());
        }
        m
    }

    /// A tool's name as the spec has it: models may use the wire form
    /// (`world__say` for `world.say`, as OpenAI-compatible APIs require).
    fn canonical(&self, name: &str) -> String {
        if self.spec.tools.iter().any(|t| t.name == name) {
            return name.to_string();
        }
        match self.spec.tools.iter().find(|t| t.name.replace('.', "__") == name) {
            Some(t) => t.name.clone(),
            None => name.to_string(),
        }
    }

    /// Lazy tools not loaded yet.
    pub fn unloaded(&self) -> Vec<&ToolDef> {
        self.spec.tools.iter().filter(|t| self.spec.lazy.contains(&t.name) && !self.loaded.contains(&t.name)).collect()
    }

    /// The tools for the next LLM call: eager and loaded ones, plus
    /// `load_tools` (with a catalogue of the rest) while any are left.
    pub fn offered_tools(&self) -> Vec<ToolDef> {
        let lazy = self.unloaded();
        let mut tools: Vec<ToolDef> = self.spec.tools.iter().filter(|t| !lazy.iter().any(|l| l.name == t.name)).cloned().collect();
        if !lazy.is_empty() {
            let catalogue: Vec<String> = lazy.iter().map(|t| format!("- {}: {}", t.name, first_sentence(&t.description))).collect();
            tools.push(ToolDef {
                name: LOAD_TOOLS.into(),
                description: format!(
                    "Load tools before using them: pass tool names, or a server name for all its tools. Their full descriptions and parameters are then available. Not loaded yet:\n{}",
                    catalogue.join("\n")
                ),
                parameters: json!({"type":"object","properties":{"names":{"type":"array","items":{"type":"string"}}},"required":["names"]}),
            });
        }
        tools
    }

    /// Loads lazy tools by name, or a whole server by `<mcp>`; returns what
    /// was loaded and what's unknown.
    fn load(&mut self, names: &[String]) -> (Vec<String>, Vec<String>) {
        let (mut loaded, mut unknown) = (vec![], vec![]);
        for n in names {
            let n = &self.canonical(n).replace("__", ".");
            let matching: Vec<String> = self
                .spec
                .lazy
                .iter()
                .filter(|t| *t == n || t.split_once('.').is_some_and(|(server, _)| server == n))
                .cloned()
                .collect();
            if matching.is_empty() {
                if !self.spec.tools.iter().any(|t| &t.name == n) {
                    unknown.push(n.clone());
                }
                continue;
            }
            for t in matching {
                if self.loaded.insert(t.clone()) {
                    loaded.push(t);
                }
            }
        }
        (loaded, unknown)
    }

    fn load_call(&mut self, call: &ToolCall) -> String {
        #[derive(Deserialize)]
        struct Args {
            names: Vec<String>,
        }
        let args: Args = match serde_json::from_str(&call.function.arguments) {
            Ok(a) => a,
            Err(e) => return format!("error: bad arguments: {e}"),
        };
        let (loaded, unknown) = self.load(&args.names);
        let mut out = if loaded.is_empty() { "nothing new to load".to_string() } else { format!("loaded: {}", loaded.join(", ")) };
        if !unknown.is_empty() {
            out.push_str(&format!("; unknown: {}", unknown.join(", ")));
        }
        out
    }

    /// May new work start? `Safe` only blocks starting a new turn.
    fn gate(&self) -> bool {
        match self.pause {
            None => true,
            Some(PauseMode::Safe) => self.phase != Phase::Idle,
            Some(_) => false,
        }
    }

    fn terminal(&self) -> bool {
        matches!(self.phase, Phase::Failed { .. } | Phase::Cancelled)
    }

    pub fn apply(&mut self, ev: &Event) -> Vec<Effect> {
        let mut fx = vec![];
        let thinking = matches!(self.phase, Phase::Thinking { .. });
        match ev {
            // Late stream events (after cancel/fail) are dropped.
            Event::LlmDelta { .. } | Event::LlmDone | Event::LlmAborted | Event::LlmFailed { .. } if !thinking => {}
            Event::Inbox { from, content, reply } => {
                self.inbox.push_back(Queued { from: from.clone(), content: content.clone(), reply: *reply });
                if self.phase == Phase::Idle {
                    self.advance(&mut fx);
                }
            }
            Event::LlmDelta { delta } => self.acc.push(delta),
            Event::LlmDone => {
                if let Some(u) = self.acc.usage.take() {
                    self.usage.prompt_tokens += u.prompt_tokens;
                    self.usage.completion_tokens += u.completion_tokens;
                }
                let mut msg = self.acc.finish();
                self.acc = Accumulator::default();
                for c in &mut msg.tool_calls {
                    c.function.name = self.canonical(&c.function.name);
                }
                let calls: Vec<_> = msg
                    .tool_calls
                    .iter()
                    .map(|call| PendingCall { call: call.clone(), state: CallState::Queued { retry: false } })
                    .collect();
                let content = msg.content.clone().unwrap_or_default();
                self.messages.push(msg);
                if calls.is_empty() {
                    self.end_turn(Status::Idle, content, &mut fx);
                } else {
                    self.phase = Phase::Tools { calls };
                }
                self.advance(&mut fx);
            }
            Event::LlmAborted => {
                if let Phase::Thinking { running } = &mut self.phase {
                    *running = false;
                }
                self.acc.tool_calls.clear();
            }
            Event::LlmFailed { error } => {
                self.acc.tool_calls.clear();
                self.fail(error.clone(), &mut fx);
            }
            Event::ToolResult { call_id, content, .. } => {
                if self.call_in(call_id, &CallState::Running) {
                    self.tool_done(call_id, content.clone(), &mut fx);
                }
            }
            Event::ToolAborted { call_id } => {
                if self.call_in(call_id, &CallState::Running) {
                    self.tool_done(call_id, "[aborted before completion]".into(), &mut fx);
                }
            }
            Event::Approval { call_id, approved } => {
                if self.call_in(call_id, &CallState::Approval) {
                    if *approved {
                        self.set_call(call_id, CallState::Approved);
                        self.advance(&mut fx);
                    } else {
                        self.tool_done(call_id, "[denied by user]".into(), &mut fx);
                    }
                }
            }
            Event::ChildSpawned { id, reserved } => {
                self.children.entry(*id).or_default();
                self.reserved += reserved;
            }
            Event::ChildReport { id, status, content } => {
                self.children.entry(*id).or_default().push(Report { status: *status, content: content.clone() });
                if matches!(self.phase, Phase::Idle | Phase::Tools { .. }) {
                    self.advance(&mut fx);
                }
            }
            Event::PauseRequested { mode } => {
                if self.terminal() {
                    return fx;
                }
                self.pause = Some(self.pause.map_or(*mode, |m| m.max(*mode)));
                if *mode == PauseMode::Hard && self.inflight() {
                    fx.push(Effect::AbortInflight);
                }
            }
            Event::Resumed => {
                self.pause = None;
                if let Phase::Failed { .. } = self.phase {
                    // Retry from where it failed: continue the (possibly partial) answer.
                    self.phase = Phase::Thinking { running: false };
                }
                self.rerequest_approvals(&mut fx);
                self.advance(&mut fx);
            }
            Event::Recovered => fx = self.recover(),
            Event::ToolsLoaded { names } => {
                self.load(names);
            }
            Event::Cancelled => {
                if self.terminal() {
                    return fx;
                }
                if self.inflight() {
                    fx.push(Effect::AbortInflight);
                }
                self.phase = Phase::Cancelled;
                self.report(Status::Cancelled, "[cancelled]".into(), &mut fx);
            }
        }
        fx
    }

    /// Emits the next effect(s) the current phase needs, if the gate allows.
    fn advance(&mut self, fx: &mut Vec<Effect>) {
        loop {
            if self.terminal() || !self.gate() {
                return;
            }
            match &mut self.phase {
                Phase::Idle => {
                    if self.inbox.is_empty() && !self.children.values().any(|r| !r.is_empty()) {
                        return;
                    }
                    self.phase = Phase::Thinking { running: false };
                }
                Phase::Thinking { running: true } => return,
                Phase::Thinking { running: false } => {
                    self.inject_pending();
                    if self.remaining_tokens() == Some(0) {
                        let used = self.usage.total() + self.reserved;
                        self.fail(format!("token budget exhausted ({used} used or reserved for children)"), fx);
                        return;
                    }
                    self.phase = Phase::Thinking { running: true };
                    fx.push(Effect::CallLlm);
                    return;
                }
                Phase::Tools { calls } => {
                    // Start every call that can start; they run in parallel.
                    let mut errors = vec![];
                    let mut loads = vec![];
                    for c in calls.iter_mut() {
                        match c.state.clone() {
                            CallState::Approved => {
                                c.state = CallState::Running;
                                fx.push(Effect::CallTool { call: c.call.clone(), retry: false });
                            }
                            CallState::Queued { retry } if c.call.function.name == WAIT_FOR => {
                                match Self::wait_ids(&self.children, &c.call) {
                                    Ok(ids) => c.state = CallState::Children { ids },
                                    Err(e) => errors.push((c.call.id.clone(), e)),
                                }
                                let _ = retry;
                            }
                            CallState::Queued { .. } if c.call.function.name == LOAD_TOOLS => loads.push(c.call.clone()),
                            // Called without its schema: load it, and have the model call again.
                            CallState::Queued { .. } if self.spec.lazy.contains(&c.call.function.name) && !self.loaded.contains(&c.call.function.name) => {
                                loads.push(c.call.clone())
                            }
                            CallState::Queued { retry: false } if self.spec.approve.contains(&c.call.function.name) => {
                                c.state = CallState::Approval;
                                fx.push(Effect::RequestApproval { call: c.call.clone() });
                            }
                            CallState::Queued { retry } => {
                                c.state = CallState::Running;
                                fx.push(Effect::CallTool { call: c.call.clone(), retry });
                            }
                            _ => {}
                        }
                    }
                    for (id, e) in errors {
                        self.finish_call(&id, e);
                    }
                    for call in loads {
                        let result = if call.function.name == LOAD_TOOLS {
                            self.load_call(&call)
                        } else {
                            self.load(std::slice::from_ref(&call.function.name));
                            format!("error: {} wasn't loaded, so it didn't run. It is loaded now; call it again with its parameters.", call.function.name)
                        };
                        self.finish_call(&call.id, result);
                    }
                    self.finish_waits();
                    let Phase::Tools { calls } = &self.phase else { unreachable!() };
                    if !calls.iter().all(|c| c.state == CallState::Done) {
                        return;
                    }
                    self.phase = Phase::Thinking { running: false };
                }
                Phase::Failed { .. } | Phase::Cancelled => return,
            }
        }
    }

    fn call_in(&self, call_id: &str, state: &CallState) -> bool {
        matches!(&self.phase, Phase::Tools { calls } if calls.iter().any(|c| c.call.id == call_id && c.state == *state))
    }

    fn set_call(&mut self, call_id: &str, state: CallState) {
        if let Phase::Tools { calls } = &mut self.phase
            && let Some(c) = calls.iter_mut().find(|c| c.call.id == call_id)
        {
            c.state = state;
        }
    }

    /// Marks a call done and records its result in the transcript.
    fn finish_call(&mut self, call_id: &str, content: String) {
        self.set_call(call_id, CallState::Done);
        self.messages.push(Message::tool(call_id, content));
    }

    fn wait_ids(children: &BTreeMap<AgentId, Vec<Report>>, call: &ToolCall) -> Result<Vec<AgentId>, String> {
        #[derive(Deserialize)]
        struct Args {
            ids: Vec<Addr>,
        }
        let args: Args =
            serde_json::from_str(&call.function.arguments).map_err(|e| format!("error: bad arguments: {e}"))?;
        let mut ids = vec![];
        for a in args.ids {
            match a {
                Addr::Agent(id) if children.contains_key(&id) => ids.push(id),
                other => return Err(format!("error: {other} is not a child of this agent")),
            }
        }
        if ids.is_empty() {
            return Err("error: ids must not be empty".into());
        }
        Ok(ids)
    }

    /// Completes every `wait_for` whose children have all reported.
    fn finish_waits(&mut self) {
        let Phase::Tools { calls } = &self.phase else { return };
        let ready: Vec<(String, Vec<AgentId>)> = calls
            .iter()
            .filter_map(|c| match &c.state {
                CallState::Children { ids } => Some((c.call.id.clone(), ids.clone())),
                _ => None,
            })
            .collect();
        for (call_id, ids) in ready {
            if !ids.iter().all(|id| self.children.get(id).is_some_and(|r| !r.is_empty())) {
                continue;
            }
            let mut out = serde_json::Map::new();
            for id in ids {
                let reports = std::mem::take(self.children.get_mut(&id).unwrap());
                out.insert(id.to_string(), json!(reports));
            }
            self.finish_call(&call_id, Value::Object(out).to_string());
        }
    }

    fn tool_done(&mut self, call_id: &str, content: String, fx: &mut Vec<Effect>) {
        self.finish_call(call_id, content);
        self.advance(fx);
    }

    /// Moves queued input into the transcript right before an LLM call.
    fn inject_pending(&mut self) {
        for Queued { from, content, reply } in std::mem::take(&mut self.inbox) {
            let kind = if reply { "reply" } else { "message" };
            self.messages.push(Message::user(match &from {
                Addr::User(_) if !reply => content,
                other => format!("[{kind} from {other}]\n{content}"),
            }));
            if !reply {
                self.reply_to.insert(from);
            }
        }
        for (id, reports) in &mut self.children {
            for r in std::mem::take(reports) {
                let status = serde_json::to_value(r.status).unwrap();
                let status = status.as_str().unwrap();
                self.messages.push(Message::user(format!("[report from agent:{id} ({status})]\n{}", r.content)));
            }
        }
    }

    fn end_turn(&mut self, status: Status, content: String, fx: &mut Vec<Effect>) {
        self.phase = Phase::Idle;
        self.report(status, content, fx);
    }

    fn fail(&mut self, error: String, fx: &mut Vec<Effect>) {
        self.phase = Phase::Failed { error: error.clone() };
        self.report(Status::Failed, error, fx);
    }

    fn report(&mut self, status: Status, content: String, fx: &mut Vec<Effect>) {
        if !self.reply_to.is_empty() {
            self.askers = self.reply_to.clone();
        }
        // A failed turn can be resumed, so its askers still await the answer.
        if status != Status::Failed {
            self.reply_to.clear();
        }
        let mut to = self.askers.clone();
        if let Some(p) = self.spec.parent {
            to.insert(Addr::Agent(p));
        }
        fx.push(Effect::Report { to: to.into_iter().collect(), status, content });
    }
}

/// Up to the first sentence end (or 120 characters) of a description.
fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    let cut: String = s[..end].chars().take(120).collect();
    if cut.len() < s[..end].len() { format!("{cut}…") } else { cut }
}

#[cfg(test)]
mod tests;
