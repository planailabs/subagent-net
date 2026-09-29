//! The agent state machine. An agent is a fold over its committed event log:
//! `apply` consumes one event and returns the side effects to perform, and
//! `replay` + `recover` rebuild a live agent after a crash or a move to
//! another spawner. No I/O happens here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::addr::{Addr, AgentId};
use crate::chat::{Accumulator, Delta, Message, ToolCall, Usage};

/// Name of the one built-in tool the state machine handles itself.
pub const WAIT_FOR: &str = "wait_for";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// `name@hash` of the agent type.
    pub ty: String,
    #[serde(default)]
    pub parent: Option<AgentId>,
    #[serde(default)]
    pub budget: Budget,
    /// Tool names that need user approval before running.
    #[serde(default)]
    pub approve: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PauseMode {
    /// Finish the current turn, then stop.
    Safe,
    /// Finish the in-flight LLM stream or tool call, then stop.
    Quick,
    /// Abort whatever is in flight now; partial output is kept.
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    Inbox { from: Addr, content: String },
    LlmDelta { delta: Delta },
    LlmDone,
    LlmAborted,
    LlmFailed { error: String },
    ToolResult { call_id: String, content: String, #[serde(default)] is_error: bool },
    ToolAborted { call_id: String },
    Approval { call_id: String, approved: bool },
    /// `reserved` tokens are carved out of this agent's budget for the child.
    ChildSpawned { id: AgentId, #[serde(default)] reserved: u64 },
    ChildReport { id: AgentId, status: Status, content: String },
    PauseRequested { mode: PauseMode },
    Resumed,
    Cancelled,
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
    /// The turn ended: deliver the final answer to everyone who asked.
    Report { to: Vec<Addr>, status: Status, content: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolWait {
    Ready { retry: bool },
    Running,
    Approval,
    /// Approved by the user, not started yet (e.g. paused meanwhile).
    Approved,
    Children { ids: Vec<AgentId> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Thinking { running: bool },
    /// Tool calls of the last assistant message; the front one is current.
    Tools { queue: VecDeque<ToolCall>, wait: ToolWait },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub inbox: VecDeque<(Addr, String)>,
    /// Who gets this turn's final answer.
    pub reply_to: BTreeSet<Addr>,
    /// Children and their reports not yet shown to the model.
    pub children: BTreeMap<AgentId, Vec<Report>>,
    pub usage: Usage,
    /// Tokens handed to children.
    pub reserved: u64,
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
            children: BTreeMap::new(),
            usage: Usage::default(),
            reserved: 0,
        }
    }

    /// Folds a log without performing effects.
    pub fn replay<'a>(id: AgentId, spec: Spec, events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut a = Self::new(id, spec);
        for e in events {
            a.apply(e);
        }
        a
    }

    /// After a replay, whatever was in flight is gone: mark it so and return the
    /// effects that continue from here.
    pub fn recover(&mut self) -> Vec<Effect> {
        match &mut self.phase {
            Phase::Thinking { running } => *running = false,
            Phase::Tools { wait: w @ ToolWait::Running, .. } => *w = ToolWait::Ready { retry: true },
            _ => {}
        }
        // Tool-call fragments of a dead stream can't be continued.
        self.acc.tool_calls.clear();
        let mut fx = vec![];
        if let (Phase::Tools { queue, wait: ToolWait::Approval }, true) = (&self.phase, self.gate()) {
            fx.push(Effect::RequestApproval { call: queue[0].clone() });
        }
        self.advance(&mut fx);
        fx
    }

    /// Tokens left for this agent and future children; `None` = unlimited.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.spec.budget.max_tokens.map(|m| m.saturating_sub(self.usage.total() + self.reserved))
    }

    /// True when something is running on the spawner for this agent.
    pub fn inflight(&self) -> bool {
        matches!(self.phase, Phase::Thinking { running: true } | Phase::Tools { wait: ToolWait::Running, .. })
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
            Event::Inbox { from, content } => {
                self.inbox.push_back((from.clone(), content.clone()));
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
                let msg = self.acc.finish();
                self.acc = Accumulator::default();
                let calls: VecDeque<_> = msg.tool_calls.iter().cloned().collect();
                let content = msg.content.clone().unwrap_or_default();
                self.messages.push(msg);
                if calls.is_empty() {
                    self.end_turn(Status::Idle, content, &mut fx);
                } else {
                    self.phase = Phase::Tools { queue: calls, wait: ToolWait::Ready { retry: false } };
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
            Event::ToolResult { call_id, content, .. } => self.tool_done(call_id, content.clone(), &mut fx),
            Event::ToolAborted { call_id } => self.tool_done(call_id, "[aborted before completion]".into(), &mut fx),
            Event::Approval { call_id, approved } => {
                if let Phase::Tools { queue, wait: w @ ToolWait::Approval } = &mut self.phase
                    && queue[0].id == *call_id
                {
                    if *approved {
                        *w = ToolWait::Approved;
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
                self.try_finish_wait(&mut fx);
                if self.phase == Phase::Idle {
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
                if let (Phase::Tools { queue, wait: ToolWait::Approval }, true) = (&self.phase, self.gate()) {
                    fx.push(Effect::RequestApproval { call: queue[0].clone() });
                }
                self.advance(&mut fx);
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
            if self.terminal() || self.inflight() || !self.gate() {
                return;
            }
            match &mut self.phase {
                Phase::Idle => {
                    if self.inbox.is_empty() && !self.children.values().any(|r| !r.is_empty()) {
                        return;
                    }
                    self.phase = Phase::Thinking { running: false };
                }
                Phase::Thinking { .. } => {
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
                Phase::Tools { queue, .. } if queue.is_empty() => self.phase = Phase::Thinking { running: false },
                Phase::Tools { queue, wait } => match wait {
                    ToolWait::Running | ToolWait::Approval | ToolWait::Children { .. } => return,
                    ToolWait::Approved => {
                        let call = queue[0].clone();
                        *wait = ToolWait::Running;
                        fx.push(Effect::CallTool { call, retry: false });
                        return;
                    }
                    ToolWait::Ready { retry } => {
                        let call = queue[0].clone();
                        let retry = *retry;
                        if call.function.name == WAIT_FOR {
                            match Self::wait_ids(&self.children, &call) {
                                Ok(ids) => *wait = ToolWait::Children { ids },
                                Err(e) => {
                                    queue.pop_front();
                                    self.messages.push(Message::tool(&call.id, e));
                                    continue;
                                }
                            }
                            if !self.try_finish_wait(fx) {
                                return;
                            }
                        } else if !retry && self.spec.approve.contains(&call.function.name) {
                            *wait = ToolWait::Approval;
                            fx.push(Effect::RequestApproval { call });
                            return;
                        } else {
                            *wait = ToolWait::Running;
                            fx.push(Effect::CallTool { call, retry });
                            return;
                        }
                    }
                },
                Phase::Failed { .. } | Phase::Cancelled => return,
            }
        }
    }

    fn wait_ids(children: &BTreeMap<AgentId, Vec<Report>>, call: &ToolCall) -> Result<Vec<AgentId>, String> {
        #[derive(Deserialize)]
        struct Args {
            ids: Vec<Addr>,
        }
        let args: Args = serde_json::from_str(&call.function.arguments).map_err(|e| format!("error: bad arguments: {e}"))?;
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

    /// Completes a `wait_for` once every awaited child has reported. Returns
    /// true if it did (the caller should keep advancing).
    fn try_finish_wait(&mut self, fx: &mut Vec<Effect>) -> bool {
        let Phase::Tools { queue, wait: ToolWait::Children { ids } } = &self.phase else { return false };
        if !ids.iter().all(|id| self.children.get(id).is_some_and(|r| !r.is_empty())) {
            return false;
        }
        let call_id = queue[0].id.clone();
        let mut out = serde_json::Map::new();
        for id in ids.clone() {
            let reports = std::mem::take(self.children.get_mut(&id).unwrap());
            out.insert(id.to_string(), json!(reports));
        }
        self.tool_done(&call_id, Value::Object(out).to_string(), fx);
        true
    }

    fn tool_done(&mut self, call_id: &str, content: String, fx: &mut Vec<Effect>) {
        let Phase::Tools { queue, wait } = &mut self.phase else { return };
        if queue.front().is_none_or(|c| c.id != call_id) {
            return;
        }
        queue.pop_front();
        *wait = ToolWait::Ready { retry: false };
        self.messages.push(Message::tool(call_id, content));
        self.advance(fx);
    }

    /// Moves queued input into the transcript right before an LLM call.
    fn inject_pending(&mut self) {
        for (from, content) in std::mem::take(&mut self.inbox) {
            self.messages.push(Message::user(match &from {
                Addr::User => content,
                other => format!("[message from {other}]\n{content}"),
            }));
            self.reply_to.insert(from);
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
        let mut to = std::mem::take(&mut self.reply_to);
        if let Some(p) = self.spec.parent {
            to.insert(Addr::Agent(p));
        }
        fx.push(Effect::Report { to: to.into_iter().collect(), status, content });
    }
}

#[cfg(test)]
mod tests;
