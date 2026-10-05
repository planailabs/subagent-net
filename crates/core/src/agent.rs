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
use crate::hooks::{Decision, HookPoint, HookSpec, Outcome};

/// How a compaction's summary is written.
pub const COMPACT_PROMPT: &str = "You compact an AI agent's conversation. The agent will continue its work from your summary plus its most recent messages, which it keeps verbatim; the rest is gone. Write the summary for the agent itself, as its own notes:\n\
- the task, its constraints and who asked;\n\
- everything found, decided or produced so far: facts, numbers, names, ids (agents, entities, documents, calls), URLs, DOIs, quotes the agent may cite, exactly as they appeared;\n\
- what was done and with what result, including what failed and why;\n\
- child agents: their ids, what each was asked, and what each reported;\n\
- open threads and the next steps the agent was about to take.\n\
Drop small talk, repetition and raw tool output that no longer matters. Be complete rather than short, but don't pad. Answer with the summary only.";

/// What a summary must be, whatever the instructions: appended to a type's
/// own `prompt` (the built-in `COMPACT_PROMPT` says it already).
pub const COMPACT_CONTRACT: &str = "The agent will continue from your summary plus its most recent messages, which it keeps verbatim; the rest is gone. Answer with the summary only.";

/// Messages kept verbatim by a compaction asked for when the type has
/// compaction off.
pub const DEFAULT_KEEP: usize = 8;

/// Name of the one built-in tool the state machine handles itself.
pub const WAIT_FOR: &str = "wait_for";
/// Loads lazy tools' schemas; resolved by the state machine itself.
pub const LOAD_TOOLS: &str = "load_tools";
/// Calls a lazy tool; its schema came into the conversation when it was loaded.
pub const CALL_TOOL: &str = "call_tool";

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
    /// How much a prompt token served from the provider's cache counts
    /// towards `max_tokens`, in percent (unset: 100). Cached input costs a
    /// fraction (DeepSeek: about a tenth), so 10 makes budgets track cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_percent: Option<u32>,
}

impl Budget {
    pub fn used(&self, u: &Usage) -> u64 {
        u.weighted(self.cached_percent.unwrap_or(100))
    }
}

/// An event on one line, for a grouped delivery: JSON compacted, anything
/// else with its line breaks escaped.
fn event_line(content: &str) -> String {
    match serde_json::from_str::<Value>(content) {
        Ok(v) => v.to_string(),
        Err(_) => content.replace('\n', "\\n"),
    }
}

/// An agent's whole conversation (`Agent::full_history`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FullHistory {
    pub messages: Vec<Message>,
    pub compactions: Vec<Compaction>,
}

/// One compaction in a full history: `messages[from..to]` were replaced, for
/// the model, by `summary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Compaction {
    pub from: usize,
    pub to: usize,
    pub summary: String,
}

/// When and how an agent's conversation is compacted: once a model call's
/// context reaches `at_tokens`, everything but the task (the first message)
/// and the last `keep` messages is replaced by a summary the model writes,
/// following `COMPACT_PROMPT` and the type's own `instructions`, if any, or
/// the type's own `prompt` in its place (with `COMPACT_CONTRACT`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Compact {
    pub at_tokens: u64,
    pub keep: usize,
    /// What summaries of this agent must also keep (or may drop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Instructions replacing `COMPACT_PROMPT` for this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// What a model can see (an agent type's `vision`): images from tools are
/// converted to one of `formats` and scaled to at most `max_px` on their
/// longer side, and the last `keep` of them go with each call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Vision {
    /// Image types the model accepts: png, jpeg, webp, gif.
    pub formats: Vec<String>,
    pub max_px: u32,
    pub keep: usize,
}

impl Default for Vision {
    fn default() -> Self {
        Self { formats: vec!["png".into(), "jpeg".into(), "webp".into(), "gif".into()], max_px: 1568, keep: 3 }
    }
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
    /// Compaction; `None` = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<Compact>,
    /// Whose work this is (set when spawned from outside, inherited by
    /// children). Per-tenant MCP servers run once per tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Whether (and how) its model sees images; `None` = text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<Vision>,
    /// Events from routes reach the model together: one message for a call,
    /// grouped by route (`[events from route:x]`, then one compact JSON line
    /// per event), instead of a message each.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub group_events: bool,
    /// Decisions made outside it at points of its loop (`hooks`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookSpec>,
    /// Tool results longer than this (characters) reach the model cut to it,
    /// with a note; `grep_result` searches the whole. `None`: never cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep_results: Option<usize>,
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
            compact: None,
            tenant: None,
            vision: None,
            group_events: false,
            hooks: vec![],
            grep_results: None,
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
    /// Moved to a new agent (an upgrade): it stops like a cancel, but
    /// reports nothing, since its work goes on in `by`.
    Superseded {
        by: AgentId,
    },
    /// The agent was (re)placed on a spawner: whatever was in flight before is
    /// gone. Logged so every replica folds the same state.
    Recovered,
    /// Lazy tools loaded ahead of a message (the hub's router).
    ToolsLoaded {
        names: Vec<String>,
    },
    /// Its MCP servers (the same versions) offer other tools now: what's
    /// offered to the model from here on.
    ToolsChanged {
        tools: Vec<ToolDef>,
        #[serde(default)]
        idempotent: Vec<String>,
        #[serde(default)]
        lazy: Vec<String>,
        /// The ones it didn't have (the model is told of them).
        #[serde(default)]
        added: Vec<String>,
    },
    /// The conversation's messages `1..upto` summarised (`Effect::Compact`).
    Compacted {
        upto: usize,
        summary: String,
        #[serde(default)]
        usage: Option<Usage>,
    },
    /// Compaction didn't work out; the agent goes on uncompacted.
    CompactFailed {
        error: String,
    },
    /// Someone asked for a compaction now (`compact`), whatever the
    /// context's size: an idle agent compacts at once and stays idle; a busy
    /// one before its next model call.
    CompactRequested,
    /// A hook's answer (`Effect::RunHook` with this `id`).
    HookDone {
        id: String,
        outcome: Outcome,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    /// Stream a completion for `Agent::llm_messages()`; propose `LlmDelta`s then
    /// `LlmDone`, `LlmAborted` or `LlmFailed`.
    CallLlm,
    /// Summarise the conversation up to `upto` (`Agent::compaction_request`);
    /// propose `Compacted`, or `CompactFailed` (`LlmAborted` when aborted).
    Compact { upto: usize },
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
    /// Run a hook; propose `HookDone` with `id`. The hub performs this from
    /// its own replica.
    RunHook { id: String, hook: HookSpec, input: Value },
}

/// Hooks run one after the other for one decision (a tool call, a message,
/// a turn's end): which ones (indices into the spec's), which is running,
/// and what they're asked about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookChain {
    pub hooks: Vec<usize>,
    pub at: usize,
    /// The chain's number: the running hook's id is `h<run>.<at>`.
    pub run: u64,
    pub input: Value,
}

impl HookChain {
    pub fn id(&self) -> String {
        format!("h{}.{}", self.run, self.at)
    }
}

/// A message held until its `on_message` hooks pass it (in order: messages
/// behind it wait too).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screened {
    pub q: Queued,
    pub chain: Option<HookChain>,
}

/// What a `Phase::Hooking` waits to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "then", rename_all = "snake_case")]
pub enum HookAt {
    /// End the turn with this answer (`on_turn_end`, then `on_report`).
    TurnEnd { content: String },
    /// Compact up to here (`pre_compact`).
    Compact { upto: usize },
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
    /// Its `pre_tool` hooks are deciding.
    PreHook {
        chain: HookChain,
    },
    /// Its `pre_tool` hooks let it run (approval may still come).
    Cleared,
    /// It returned; its `post_tool` hooks are deciding about the result.
    PostHook {
        chain: HookChain,
        result: String,
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
    /// Hooks deciding before the turn ends or a compaction runs.
    Hooking {
        chain: HookChain,
        at: HookAt,
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
    /// Schemas the router loaded, shown to the model with the next message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Context size of the last model call (prompt + completion tokens):
    /// what compaction is decided on.
    #[serde(default)]
    pub context: u64,
    /// A compaction failed this step: go on without one until the next call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compact_skip: bool,
    /// How often the conversation was compacted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compactions: u32,
    /// A compaction was asked for (`CompactRequested`) and is still to come.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compact_asked: bool,
    /// The compaction under way was asked for while idle: idle again after.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compact_then_idle: bool,
    /// The agent its work moved to (an upgrade), once it's superseded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<AgentId>,
    /// Messages its `on_message` hooks hold (in order).
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub screening: VecDeque<Screened>,
    /// Hook chains started (their ids' numbers).
    #[serde(default, skip_serializing_if = "is_zero64")]
    pub hook_runs: u64,
    /// How often `on_turn_end` hooks continued this turn.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub continues: u32,
    /// `pre_compact` hooks' instructions for the coming summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compact_notes: Vec<String>,
}

fn is_zero64(n: &u64) -> bool {
    *n == 0
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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
            notes: vec![],
            context: 0,
            compact_skip: false,
            compactions: 0,
            compact_asked: false,
            compact_then_idle: false,
            superseded_by: None,
            screening: VecDeque::new(),
            hook_runs: 0,
            continues: 0,
            compact_notes: vec![],
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

    /// The whole conversation as it happened, from a log: the messages in
    /// order, compacted ones included (each compaction's own notes, its
    /// summary and reloaded schemas, left out), and where each compaction
    /// was. Compaction only changes what the model is sent; the log keeps
    /// everything, and this is how to read it back.
    pub fn full_history<'a>(id: AgentId, spec: Spec, events: impl IntoIterator<Item = &'a Event>) -> FullHistory {
        let mut a = Self::new(id, spec);
        let mut earlier: Vec<Message> = vec![];
        let mut compactions = vec![];
        // How many of the messages after the task are a compaction's notes.
        let mut notes = 0;
        for e in events {
            if let Event::Compacted { upto, summary, .. } = e
                && (2..=a.messages.len()).contains(upto)
            {
                let gone = &a.messages[(1 + notes).min(*upto)..*upto];
                let from = 1 + earlier.len();
                earlier.extend(gone.iter().cloned());
                compactions.push(Compaction { from, to: 1 + earlier.len(), summary: summary.clone() });
                let kept = a.messages.len() - upto;
                a.apply(e);
                notes = a.messages.len() - 1 - kept;
                continue;
            }
            a.apply(e);
        }
        let mut messages: Vec<Message> = a.messages.first().cloned().into_iter().collect();
        messages.extend(earlier);
        messages.extend(a.messages.iter().skip(1 + notes).cloned());
        FullHistory { messages, compactions }
    }

    /// Whatever was in flight is gone: mark it so and return the effects that
    /// continue from here. Applied via `Event::Recovered`.
    /// After a compaction (or a failed one): on to the model call it came
    /// before, or idle again if it was asked for while idle.
    fn after_compaction(&mut self, fx: &mut Vec<Effect>) {
        self.phase = if std::mem::take(&mut self.compact_then_idle) { Phase::Idle } else { Phase::Thinking { running: false } };
        self.advance(fx);
    }

    fn recover(&mut self) -> Vec<Effect> {
        // A compaction asked for while idle is asked for again, nothing more.
        if self.compact_then_idle && matches!(self.phase, Phase::Thinking { .. }) {
            self.acc = Accumulator::default();
            return match self.compact_point() {
                Some(upto) => {
                    self.phase = Phase::Thinking { running: true };
                    vec![Effect::Compact { upto }]
                }
                None => {
                    self.compact_then_idle = false;
                    self.phase = Phase::Idle;
                    let mut fx = vec![];
                    self.advance(&mut fx);
                    fx
                }
            };
        }
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
        // Hooks that were running: run again if that's safe, else their
        // on_lost decides.
        let mut lost = vec![];
        for c in self.hook_chains() {
            if self.hook_of(&c).idempotent {
                self.run_hook(&c, &mut fx);
            } else {
                lost.push(c.id());
            }
        }
        for id in lost {
            self.hook_done(&id, Outcome::error("its runner stopped before it answered"), &mut fx);
        }
        self.advance(&mut fx);
        fx
    }

    /// Every hook chain waiting for an answer.
    fn hook_chains(&self) -> Vec<HookChain> {
        let mut out = vec![];
        if let Phase::Tools { calls } = &self.phase {
            for c in calls {
                if let CallState::PreHook { chain } | CallState::PostHook { chain, .. } = &c.state {
                    out.push(chain.clone());
                }
            }
        }
        if let Some(c) = self.screening.front().and_then(|s| s.chain.clone()) {
            out.push(c);
        }
        if let Phase::Hooking { chain, .. } = &self.phase {
            out.push(chain.clone());
        }
        out
    }

    /// The hooks it waits for: (the run's id, the hook's name).
    pub fn waiting_hooks(&self) -> Vec<(String, String)> {
        self.hook_chains().iter().map(|c| (c.id(), self.hook_of(c).name.clone())).collect()
    }

    fn chain(&mut self, hooks: Vec<usize>, input: Value) -> HookChain {
        self.hook_runs += 1;
        HookChain { hooks, at: 0, run: self.hook_runs, input }
    }

    fn hook_of(&self, c: &HookChain) -> &HookSpec {
        &self.spec.hooks[c.hooks[c.at]]
    }

    fn run_hook(&self, c: &HookChain, fx: &mut Vec<Effect>) {
        fx.push(Effect::RunHook { id: c.id(), hook: self.hook_of(c).clone(), input: c.input.clone() });
    }

    /// Moves a chain to its next hook (and runs it), if there is one.
    fn next_hook(&self, c: &mut HookChain, fx: &mut Vec<Effect>) -> bool {
        c.at += 1;
        if c.at < c.hooks.len() {
            self.run_hook(c, fx);
            true
        } else {
            false
        }
    }

    /// The model ended its turn: its `on_turn_end` and `on_report` hooks
    /// first, if it has any.
    fn turn_end(&mut self, content: String, fx: &mut Vec<Effect>) {
        let mut hooks = crate::hooks::at(&self.spec.hooks, HookPoint::OnTurnEnd, "");
        hooks.extend(crate::hooks::at(&self.spec.hooks, HookPoint::OnReport, ""));
        if hooks.is_empty() {
            return self.end_turn(Status::Idle, content, fx);
        }
        let chain = self.chain(hooks, json!({"content": content}));
        self.run_hook(&chain, fx);
        self.phase = Phase::Hooking { chain, at: HookAt::TurnEnd { content } };
    }

    /// A compaction is due: its `pre_compact` hooks first, if it has any.
    fn begin_compaction(&mut self, upto: usize, fx: &mut Vec<Effect>) {
        let hooks = crate::hooks::at(&self.spec.hooks, HookPoint::PreCompact, "");
        if hooks.is_empty() {
            self.phase = Phase::Thinking { running: true };
            fx.push(Effect::Compact { upto });
            return;
        }
        let chain = self.chain(hooks, json!({"upto": upto, "messages": self.messages.len()}));
        self.run_hook(&chain, fx);
        self.phase = Phase::Hooking { chain, at: HookAt::Compact { upto } };
    }

    /// Lets held messages through, in order: the first one's hooks start;
    /// one without hooks goes on to the inbox.
    fn screen(&mut self, fx: &mut Vec<Effect>) {
        while let Some(front) = self.screening.front() {
            if front.chain.is_some() {
                return;
            }
            let from = front.q.from.to_string();
            let hooks = crate::hooks::at(&self.spec.hooks, HookPoint::OnMessage, &from);
            if hooks.is_empty() {
                let s = self.screening.pop_front().unwrap();
                self.inbox.push_back(s.q);
                continue;
            }
            let input = json!({"from": from, "content": front.q.content, "reply": front.q.reply});
            let chain = self.chain(hooks, input);
            self.run_hook(&chain, fx);
            self.screening.front_mut().unwrap().chain = Some(chain);
            return;
        }
    }

    /// The `post_tool` hooks for a call that returned, started (none: `None`).
    fn post_hooks(&mut self, call_id: &str, result: &str) -> Option<HookChain> {
        let run = self.run_of(call_id)?;
        let hooks = crate::hooks::at(&self.spec.hooks, HookPoint::PostTool, &run.function.name);
        if hooks.is_empty() {
            return None;
        }
        let args: Value = serde_json::from_str(&run.function.arguments).unwrap_or(Value::Null);
        Some(self.chain(hooks, json!({"tool": run.function.name, "args": args, "call_id": call_id, "result": result})))
    }

    /// A call as it runs (call_tool resolved).
    fn run_of(&self, call_id: &str) -> Option<ToolCall> {
        let Phase::Tools { calls } = &self.phase else { return None };
        let c = calls.iter().find(|c| c.call.id == call_id)?;
        match translate(&self.spec, &self.loaded, &c.call) {
            Dispatch::Run(run) => Some(run),
            _ => None,
        }
    }

    /// A pre_tool hook's new arguments for a call (inside call_tool's, for one).
    fn rewrite_args(&mut self, call_id: &str, args: &Value) {
        if let Phase::Tools { calls } = &mut self.phase
            && let Some(c) = calls.iter_mut().find(|c| c.call.id == call_id)
        {
            c.call.function.arguments = if c.call.function.name == CALL_TOOL {
                let mut outer: Value = serde_json::from_str(&c.call.function.arguments).unwrap_or_else(|_| json!({}));
                outer["arguments"] = args.clone();
                outer.to_string()
            } else {
                args.to_string()
            };
        }
    }

    /// A hook couldn't decide and its on_lost is `fail`: the agent fails.
    fn hook_failed(&mut self, h: &HookSpec, why: Option<String>, fx: &mut Vec<Effect>) {
        if self.inflight() {
            fx.push(Effect::AbortInflight);
        }
        self.acc.tool_calls.clear();
        self.fail(why.unwrap_or_else(|| format!("hook {} failed", h.name)), fx);
    }

    /// A hook answered: on with whatever it was deciding about. An answer
    /// for no hook that's waiting (late, after a cancel) changes nothing.
    fn hook_done(&mut self, id: &str, outcome: Outcome, fx: &mut Vec<Effect>) {
        if let Phase::Tools { calls } = &self.phase
            && let Some(c) = calls.iter().find(|c| matches!(&c.state, CallState::PreHook { chain } | CallState::PostHook { chain, .. } if chain.id() == id))
        {
            let call_id = c.call.id.clone();
            match c.state.clone() {
                CallState::PreHook { chain } => self.pre_tool_done(&call_id, chain, outcome, fx),
                CallState::PostHook { chain, result } => self.post_tool_done(&call_id, chain, result, outcome, fx),
                _ => unreachable!("matched above"),
            }
            return;
        }
        if self.screening.front().and_then(|s| s.chain.as_ref()).is_some_and(|c| c.id() == id) {
            return self.message_done(outcome, fx);
        }
        if matches!(&self.phase, Phase::Hooking { chain, .. } if chain.id() == id) {
            self.phase_hook_done(outcome, fx);
        }
    }

    fn pre_tool_done(&mut self, call_id: &str, mut chain: HookChain, outcome: Outcome, fx: &mut Vec<Effect>) {
        let h = self.hook_of(&chain).clone();
        let o = judged(&h, outcome);
        match o.decision {
            Decision::Allow | Decision::Rewrite => {
                if o.decision == Decision::Rewrite
                    && let Some(args) = &o.args
                {
                    self.rewrite_args(call_id, args);
                    chain.input["args"] = args.clone();
                }
                if self.next_hook(&mut chain, fx) {
                    self.set_call(call_id, CallState::PreHook { chain });
                } else {
                    self.set_call(call_id, CallState::Cleared);
                }
                self.advance(fx);
            }
            Decision::Deny => self.tool_done(call_id, format!("[denied by hook {}: {}]", h.name, o.reason.unwrap_or_else(|| "no reason given".into())), fx),
            Decision::Ask => {
                self.set_call(call_id, CallState::Approval);
                if let Some(run) = self.run_of(call_id) {
                    fx.push(Effect::RequestApproval { call: run });
                }
            }
            Decision::Continue | Decision::Error => {
                self.finish_call(call_id, format!("[{}]", o.reason.clone().unwrap_or_else(|| format!("hook {} failed", h.name))));
                self.hook_failed(&h, o.reason, fx);
            }
        }
    }

    fn post_tool_done(&mut self, call_id: &str, mut chain: HookChain, result: String, outcome: Outcome, fx: &mut Vec<Effect>) {
        let h = self.hook_of(&chain).clone();
        let o = judged(&h, outcome);
        match o.decision {
            Decision::Allow | Decision::Rewrite => {
                let mut result = if o.decision == Decision::Rewrite { o.text.unwrap_or(result) } else { result };
                if let Some(n) = o.note {
                    result = format!("{result}\n\n[note from hook {}] {n}", h.name);
                }
                chain.input["result"] = json!(result);
                if self.next_hook(&mut chain, fx) {
                    self.set_call(call_id, CallState::PostHook { chain, result });
                } else {
                    self.tool_done(call_id, result, fx);
                }
            }
            Decision::Deny => self.tool_done(call_id, format!("[result withheld by hook {}: {}]", h.name, o.reason.unwrap_or_else(|| "no reason given".into())), fx),
            _ => {
                self.finish_call(call_id, format!("[{}]", o.reason.clone().unwrap_or_else(|| format!("hook {} failed", h.name))));
                self.hook_failed(&h, o.reason, fx);
            }
        }
    }

    fn message_done(&mut self, outcome: Outcome, fx: &mut Vec<Effect>) {
        let mut s = self.screening.pop_front().expect("checked by hook_done");
        let mut chain = s.chain.take().expect("checked by hook_done");
        let h = self.hook_of(&chain).clone();
        let o = judged(&h, outcome);
        match o.decision {
            Decision::Allow | Decision::Rewrite => {
                if o.decision == Decision::Rewrite
                    && let Some(t) = o.text
                {
                    s.q.content = t;
                }
                if let Some(n) = o.note {
                    s.q.content = format!("{}\n\n[note from hook {}] {n}", s.q.content, h.name);
                }
                chain.input["content"] = json!(s.q.content);
                if self.next_hook(&mut chain, fx) {
                    s.chain = Some(chain);
                    self.screening.push_front(s);
                    return;
                }
                self.inbox.push_back(s.q);
            }
            // Dropped.
            Decision::Deny => {}
            _ => return self.hook_failed(&h, o.reason, fx),
        }
        self.screen(fx);
        if self.phase == Phase::Idle {
            self.advance(fx);
        }
    }

    fn phase_hook_done(&mut self, outcome: Outcome, fx: &mut Vec<Effect>) {
        let Phase::Hooking { mut chain, at } = std::mem::replace(&mut self.phase, Phase::Idle) else { unreachable!("checked by hook_done") };
        let h = self.hook_of(&chain).clone();
        let o = judged(&h, outcome);
        match (at, o.decision) {
            (HookAt::TurnEnd { .. }, Decision::Continue) if self.continues < h.max_continue => {
                self.continues += 1;
                self.messages.push(Message::user(format!("[from hook {}]\n{}", h.name, o.text.unwrap_or_default())));
                self.phase = Phase::Thinking { running: false };
                self.advance(fx);
            }
            (HookAt::TurnEnd { mut content }, Decision::Allow | Decision::Continue | Decision::Rewrite) => {
                if o.decision == Decision::Rewrite
                    && let Some(t) = o.text
                {
                    content = t;
                    chain.input["content"] = json!(content);
                }
                if self.next_hook(&mut chain, fx) {
                    self.phase = Phase::Hooking { chain, at: HookAt::TurnEnd { content } };
                } else {
                    self.end_turn(Status::Idle, content, fx);
                    self.advance(fx);
                }
            }
            (HookAt::Compact { upto }, Decision::Allow) => {
                if let Some(t) = o.text.filter(|t| !t.trim().is_empty()) {
                    self.compact_notes.push(t);
                }
                if self.next_hook(&mut chain, fx) {
                    self.phase = Phase::Hooking { chain, at: HookAt::Compact { upto } };
                } else {
                    self.phase = Phase::Thinking { running: true };
                    fx.push(Effect::Compact { upto });
                }
            }
            (at, _) => {
                self.phase = Phase::Hooking { chain, at };
                self.hook_failed(&h, o.reason, fx);
            }
        }
    }

    /// Asks again for approvals still open (after a restart or resume).
    fn rerequest_approvals(&self, fx: &mut Vec<Effect>) {
        if let (Phase::Tools { calls }, true) = (&self.phase, self.gate()) {
            for c in calls.iter().filter(|c| c.state == CallState::Approval) {
                fx.push(Effect::RequestApproval { call: c.call.clone() });
            }
        }
    }

    /// Tool calls waiting for approval (as they'll run: call_tool resolved).
    pub fn awaiting_approval(&self) -> Vec<ToolCall> {
        match &self.phase {
            Phase::Tools { calls } => calls
                .iter()
                .filter(|c| c.state == CallState::Approval)
                .map(|c| match translate(&self.spec, &self.loaded, &c.call) {
                    Dispatch::Run(call) => call,
                    _ => c.call.clone(),
                })
                .collect(),
            _ => vec![],
        }
    }

    /// Tokens left for this agent and future children; `None` = unlimited.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.spec.budget.max_tokens.map(|m| m.saturating_sub(self.spec.budget.used(&self.usage) + self.reserved))
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

    /// Where to compact before the next model call, if it's time: messages
    /// `1..upto` go (the first, the task, stays), the last `keep` stay, and
    /// the kept part never starts with a tool result (its call must stay
    /// with it).
    /// One asked for (`compact_asked`) needn't wait for the threshold, and
    /// works with compaction off too (keeping `DEFAULT_KEEP`).
    fn compact_point(&self) -> Option<usize> {
        let asked = self.compact_asked || self.compact_then_idle;
        let keep = match self.spec.compact.as_ref() {
            Some(c) if asked => c.keep,
            Some(c) if !self.compact_skip && self.context >= c.at_tokens => c.keep,
            None if asked => DEFAULT_KEEP,
            _ => return None,
        };
        if !self.acc.is_empty() {
            return None;
        }
        let mut upto = self.messages.len().saturating_sub(keep);
        while upto > 1 && self.messages.get(upto).is_some_and(|m| m.role == crate::chat::Role::Tool) {
            upto -= 1;
        }
        // Nothing worth summarising.
        (upto >= 3).then_some(upto)
    }

    /// The model call that writes a compaction's summary: `system` is the
    /// agent's own system prompt (what it's for), then the conversation up
    /// to `upto` as text.
    pub fn compaction_request(&self, system: &str, upto: usize) -> Vec<Message> {
        use std::fmt::Write;
        let mut t = String::new();
        for m in &self.messages[..upto.min(self.messages.len())] {
            let role = serde_json::to_value(&m.role).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            let _ = writeln!(t, "[{role}]");
            if let Some(c) = m.content.as_deref().filter(|c| !c.is_empty()) {
                let _ = writeln!(t, "{c}");
            }
            for c in &m.tool_calls {
                let _ = writeln!(t, "-> {} {} (call {})", c.function.name, c.function.arguments, c.id);
            }
            if let Some(id) = &m.tool_call_id {
                let _ = writeln!(t, "(result of call {id})");
            }
            t.push('\n');
        }
        let c = self.spec.compact.as_ref();
        let mut prompt = match c.and_then(|c| c.prompt.as_deref()) {
            Some(own) => format!("{own}\n\n{COMPACT_CONTRACT}"),
            None => COMPACT_PROMPT.to_string(),
        };
        if let Some(i) = c.and_then(|c| c.instructions.as_deref()) {
            prompt = format!("{prompt}\n\nFor this agent in particular:\n{i}");
        }
        for n in &self.compact_notes {
            prompt = format!("{prompt}\n\nFor this summary:\n{n}");
        }
        vec![
            Message::system(prompt),
            Message::user(format!("The agent's instructions:\n{system}\n\nIts conversation so far:\n\n{t}")),
        ]
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

    /// The tools offered to the model. They never change during an
    /// agent's life, so the provider's prompt cache (which covers the tool
    /// list and then the conversation) stays valid: eager tools, plus
    /// `load_tools` (a fixed catalogue of every lazy tool) and `call_tool`
    /// when there are lazy tools. Loaded schemas live in the conversation.
    pub fn offered_tools(&self) -> Vec<ToolDef> {
        let mut tools: Vec<ToolDef> = self.spec.tools.iter().filter(|t| !self.spec.lazy.contains(&t.name)).cloned().collect();
        let lazy: Vec<&ToolDef> = self.spec.tools.iter().filter(|t| self.spec.lazy.contains(&t.name)).collect();
        if !lazy.is_empty() {
            let catalogue: Vec<String> = lazy.iter().map(|t| format!("- {}: {}", t.name, first_sentence(&t.description))).collect();
            tools.push(ToolDef {
                name: LOAD_TOOLS.into(),
                description: format!(
                    "Load tools before using them: pass tool names, or a server name for all its tools. You get their full descriptions and parameters; then call them with call_tool. Available:\n{}",
                    catalogue.join("\n")
                ),
                parameters: json!({"type":"object","properties":{"names":{"type":"array","items":{"type":"string"}}},"required":["names"]}),
            });
            tools.push(ToolDef {
                name: CALL_TOOL.into(),
                description: "Call a tool you loaded with load_tools: its name and its arguments (an object matching the parameters load_tools showed).".into(),
                parameters: json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name","arguments"]}),
            });
        }
        tools
    }

    /// Lazy tools matching names (tool names, or a server name for all its
    /// tools); returns the matches (loading them) and the unknown names.
    fn load(&mut self, names: &[String]) -> (Vec<String>, Vec<String>) {
        let (mut matched, mut unknown) = (vec![], vec![]);
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
                self.loaded.insert(t.clone());
                if !matched.contains(&t) {
                    matched.push(t);
                }
            }
        }
        (matched, unknown)
    }

    /// Full schemas of tools, one JSON object per line.
    fn schemas(&self, names: &[String]) -> String {
        names
            .iter()
            .filter_map(|n| self.spec.tools.iter().find(|t| &t.name == n))
            .map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters}).to_string())
            .collect::<Vec<_>>()
            .join("\n")
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
        let (matched, unknown) = self.load(&args.names);
        let mut out = if matched.is_empty() {
            "nothing loaded".to_string()
        } else {
            format!("loaded {}; call them with call_tool {{\"name\", \"arguments\"}}:\n{}", matched.join(", "), self.schemas(&matched))
        };
        if !unknown.is_empty() {
            out.push_str(&format!("\nunknown: {}", unknown.join(", ")));
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

    /// Folds one event into the state and returns the effects it calls for.
    ///
    /// A panic here is a bug. It must not take the hub or a node down with
    /// it (every agent there shares the process), so it's caught: the agent
    /// fails with an internal error, its askers hear about it, and anything
    /// in flight is aborted. Replicas run the same code, so they fail the
    /// same way. The state isn't copied beforehand (that would cost a copy
    /// of the transcript per streamed delta); a failed agent can be resumed.
    pub fn apply(&mut self, ev: &Event) -> Vec<Effect> {
        GUARDED.with(|g| g.set(g.get() + 1));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.apply_inner(ev)));
        GUARDED.with(|g| g.set(g.get() - 1));
        match r {
            Ok(fx) => fx,
            Err(payload) => {
                let msg = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "?".into());
                let kind = serde_json::to_value(ev).ok().and_then(|v| v["type"].as_str().map(String::from)).unwrap_or_default();
                let mut fx = vec![];
                if self.inflight() {
                    fx.push(Effect::AbortInflight);
                }
                self.acc.tool_calls.clear();
                self.fail(format!("internal error applying a {kind} event: {msg} (a bug in subnet)"), &mut fx);
                fx
            }
        }
    }

    fn apply_inner(&mut self, ev: &Event) -> Vec<Effect> {
        #[cfg(test)]
        if let Event::Inbox { content, .. } = ev
            && content == "\u{0}panic"
        {
            panic!("test panic");
        }
        let mut fx = vec![];
        let thinking = matches!(self.phase, Phase::Thinking { .. });
        match ev {
            // Late stream events (after cancel/fail) are dropped.
            Event::LlmDelta { .. } | Event::LlmDone | Event::LlmAborted | Event::LlmFailed { .. } | Event::Compacted { .. } | Event::CompactFailed { .. }
                if !thinking => {}
            Event::Inbox { from, content, reply } => {
                let q = Queued { from: from.clone(), content: content.clone(), reply: *reply };
                // Held while its hooks decide, or while one before it is held.
                if !self.screening.is_empty() || !crate::hooks::at(&self.spec.hooks, HookPoint::OnMessage, &from.to_string()).is_empty() {
                    self.screening.push_back(Screened { q, chain: None });
                    self.screen(&mut fx);
                } else {
                    self.inbox.push_back(q);
                }
                if self.phase == Phase::Idle {
                    self.advance(&mut fx);
                }
            }
            Event::LlmDelta { delta } => self.acc.push(delta),
            Event::LlmDone => {
                if let Some(u) = self.acc.usage.take() {
                    self.usage.prompt_tokens += u.prompt_tokens;
                    self.usage.completion_tokens += u.completion_tokens;
                    self.usage.cached_prompt_tokens += u.cached_prompt_tokens;
                    self.context = u.prompt_tokens + u.completion_tokens;
                }
                self.compact_skip = false;
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
                    self.turn_end(content, &mut fx);
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
                    match self.post_hooks(call_id, content) {
                        Some(chain) => {
                            self.run_hook(&chain, &mut fx);
                            self.set_call(call_id, CallState::PostHook { chain, result: content.clone() });
                        }
                        None => self.tool_done(call_id, content.clone(), &mut fx),
                    }
                }
            }
            Event::HookDone { id, outcome } => self.hook_done(id, outcome.clone(), &mut fx),
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
                let fresh: Vec<String> = names.iter().map(|n| self.canonical(n)).filter(|n| self.spec.lazy.contains(n) && !self.loaded.contains(n)).collect();
                let (matched, _) = self.load(&fresh);
                if !matched.is_empty() {
                    self.notes.push(format!(
                        "[tools loaded for the next message; call them with call_tool {{\"name\", \"arguments\"}}]\n{}",
                        self.schemas(&matched)
                    ));
                }
            }
            Event::ToolsChanged { tools, idempotent, lazy, added } => {
                self.loaded.retain(|n| tools.iter().any(|t| &t.name == n));
                self.spec.tools = tools.clone();
                self.spec.idempotent = idempotent.clone();
                self.spec.lazy = lazy.clone();
                if !added.is_empty() {
                    self.notes.push(format!("[new tools you have now: {}]", added.join(", ")));
                }
            }
            Event::Compacted { upto, summary, usage } => {
                if let Some(u) = usage {
                    self.usage.prompt_tokens += u.prompt_tokens;
                    self.usage.completion_tokens += u.completion_tokens;
                    self.usage.cached_prompt_tokens += u.cached_prompt_tokens;
                }
                if (2..=self.messages.len()).contains(upto) {
                    let tail = self.messages.split_off(*upto);
                    self.messages.truncate(1);
                    self.messages.push(Message::user(format!(
                        "[The conversation so far was compacted: earlier messages were replaced by this summary.]\n{summary}"
                    )));
                    // Loaded schemas were in the compacted part; call_tool still needs them.
                    let loaded: Vec<String> = self.loaded.iter().cloned().collect();
                    if !loaded.is_empty() {
                        self.messages.push(Message::user(format!(
                            "[tools you loaded earlier; call them with call_tool {{\"name\", \"arguments\"}}]\n{}",
                            self.schemas(&loaded)
                        )));
                    }
                    self.messages.extend(tail);
                    self.compactions += 1;
                    self.context = 0;
                }
                self.compact_notes.clear();
                self.after_compaction(&mut fx);
            }
            Event::CompactFailed { .. } => {
                self.compact_skip = true;
                self.compact_notes.clear();
                self.after_compaction(&mut fx);
            }
            Event::CompactRequested => {
                if self.terminal() {
                    return fx;
                }
                self.compact_asked = true;
                // Idle: now, and idle again after; busy: before the next call.
                if self.phase == Phase::Idle {
                    self.compact_asked = false;
                    self.compact_then_idle = true;
                    match self.compact_point() {
                        Some(upto) => self.begin_compaction(upto, &mut fx),
                        None => self.compact_then_idle = false,
                    }
                }
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
            Event::Superseded { by } => {
                if self.terminal() {
                    return fx;
                }
                self.superseded_by = Some(*by);
                if self.inflight() {
                    fx.push(Effect::AbortInflight);
                }
                self.phase = Phase::Cancelled;
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
                    self.continues = 0;
                    self.phase = Phase::Thinking { running: false };
                }
                Phase::Hooking { .. } => return,
                Phase::Thinking { running: true } => return,
                Phase::Thinking { running: false } => {
                    self.inject_pending();
                    if self.remaining_tokens() == Some(0) {
                        let used = self.spec.budget.used(&self.usage) + self.reserved;
                        self.fail(format!("token budget exhausted ({used} used or reserved for children)"), fx);
                        return;
                    }
                    match self.compact_point() {
                        Some(upto) => self.begin_compaction(upto, fx),
                        None => {
                            self.phase = Phase::Thinking { running: true };
                            fx.push(Effect::CallLlm);
                        }
                    }
                    // Asked for or not, it's done (or there was nothing to compact).
                    self.compact_asked = false;
                    return;
                }
                Phase::Tools { calls } => {
                    // load_tools first, so the other calls of the same message
                    // can use what it loads.
                    let load_tools: Vec<ToolCall> = calls
                        .iter()
                        .filter(|c| c.call.function.name == LOAD_TOOLS && matches!(c.state, CallState::Queued { .. }))
                        .map(|c| c.call.clone())
                        .collect();
                    if !load_tools.is_empty() {
                        for call in load_tools {
                            let result = self.load_call(&call);
                            self.finish_call(&call.id, result);
                        }
                        continue;
                    }
                    // pre_tool hooks judge a call before it may start.
                    let judged: Vec<(String, ToolCall)> = {
                        let Phase::Tools { calls } = &self.phase else { unreachable!() };
                        calls
                            .iter()
                            .filter(|c| c.state == CallState::Queued { retry: false } && c.call.function.name != WAIT_FOR)
                            .filter_map(|c| match translate(&self.spec, &self.loaded, &c.call) {
                                Dispatch::Run(run) => Some((c.call.id.clone(), run)),
                                _ => None,
                            })
                            .collect()
                    };
                    for (id, run) in judged {
                        let hooks = crate::hooks::at(&self.spec.hooks, HookPoint::PreTool, &run.function.name);
                        if hooks.is_empty() {
                            continue;
                        }
                        let args: Value = serde_json::from_str(&run.function.arguments).unwrap_or(Value::Null);
                        let chain = self.chain(hooks, json!({"tool": run.function.name, "args": args, "call_id": id}));
                        self.run_hook(&chain, fx);
                        self.set_call(&id, CallState::PreHook { chain });
                    }
                    // Start every call that can start; they run in parallel.
                    // Every call is judged against what was loaded before any
                    // of them (two calls of one unloaded tool both get its schema).
                    let loaded = self.loaded.clone();
                    let mut errors = vec![];
                    let mut loads = vec![];
                    let Phase::Tools { calls } = &mut self.phase else { unreachable!() };
                    for c in calls.iter_mut() {
                        // What actually runs: call_tool becomes the tool it names.
                        let run = match translate(&self.spec, &loaded, &c.call) {
                            Dispatch::Run(call) => call,
                            Dispatch::Load(_) if matches!(c.state, CallState::Queued { .. }) => {
                                loads.push(c.call.clone());
                                continue;
                            }
                            Dispatch::Fail(e) if matches!(c.state, CallState::Queued { .. }) => {
                                errors.push((c.call.id.clone(), e));
                                continue;
                            }
                            _ => continue,
                        };
                        match c.state.clone() {
                            CallState::Cleared if self.spec.approve.contains(&run.function.name) => {
                                c.state = CallState::Approval;
                                fx.push(Effect::RequestApproval { call: run });
                            }
                            CallState::Cleared | CallState::Approved => {
                                c.state = CallState::Running;
                                fx.push(Effect::CallTool { call: run, retry: false });
                            }
                            CallState::Queued { retry } if c.call.function.name == WAIT_FOR => {
                                match Self::wait_ids(&self.children, &c.call) {
                                    Ok(ids) => c.state = CallState::Children { ids },
                                    Err(e) => errors.push((c.call.id.clone(), e)),
                                }
                                let _ = retry;
                            }
                            CallState::Queued { retry: false } if self.spec.approve.contains(&run.function.name) => {
                                c.state = CallState::Approval;
                                fx.push(Effect::RequestApproval { call: run });
                            }
                            CallState::Queued { retry } => {
                                c.state = CallState::Running;
                                fx.push(Effect::CallTool { call: run, retry });
                            }
                            _ => {}
                        }
                    }
                    for (id, e) in errors {
                        self.finish_call(&id, e);
                    }
                    for call in loads {
                        let Dispatch::Load(name) = translate(&self.spec, &loaded, &call) else {
                            unreachable!("judged against the same snapshot")
                        };
                        self.load(std::slice::from_ref(&name));
                        let result = format!(
                            "error: {name} wasn't loaded, so it didn't run. Here is its schema; call it again with call_tool {{\"name\", \"arguments\"}}:\n{}",
                            self.schemas(std::slice::from_ref(&name))
                        );
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
        let message = match self.spec.grep_results {
            // Searching results isn't cut again (they're paged).
            Some(over) if content.chars().count() > over && !self.is_search(call_id) => cut_result(call_id, content, over),
            _ => Message::tool(call_id, content),
        };
        self.messages.push(message);
    }

    /// A call of `grep_result` or `search_history`.
    fn is_search(&self, call_id: &str) -> bool {
        self.messages.iter().rev().flat_map(|m| &m.tool_calls).find(|c| c.id == call_id).is_some_and(|c| matches!(c.function.name.as_str(), "grep_result" | "search_history"))
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
        for note in std::mem::take(&mut self.notes) {
            self.messages.push(Message::user(note));
        }
        // Grouped events: where their message goes (where the first one
        // was), and each route's lines in order.
        let mut grouped: Option<(usize, Vec<(Addr, Vec<String>)>)> = None;
        for Queued { from, content, reply } in std::mem::take(&mut self.inbox) {
            if self.spec.group_events && !reply && matches!(from, Addr::Route(_)) {
                let (_, by_route) = grouped.get_or_insert_with(|| (self.messages.len(), vec![]));
                let line = event_line(&content);
                match by_route.iter_mut().find(|(r, _)| *r == from) {
                    Some((_, lines)) => lines.push(line),
                    None => by_route.push((from.clone(), vec![line])),
                }
                self.reply_to.insert(from);
                continue;
            }
            let kind = if reply { "reply" } else { "message" };
            self.messages.push(Message::user(match &from {
                Addr::User(_) if !reply => content,
                other => format!("[{kind} from {other}]\n{content}"),
            }));
            if !reply {
                self.reply_to.insert(from);
            }
        }
        if let Some((at, by_route)) = grouped {
            let text = by_route.iter().map(|(r, lines)| format!("[events from {r}]\n{}", lines.join("\n"))).collect::<Vec<_>>().join("\n\n");
            self.messages.insert(at, Message::user(text));
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

/// What a hook's answer comes to: a decision its point doesn't take counts
/// as an error, and an error is replaced by what its `on_lost` says (an
/// error still: `fail`).
fn judged(h: &HookSpec, o: Outcome) -> Outcome {
    let o = if o.decision != Decision::Error && !h.on.allows(o.decision) {
        Outcome::error(format!("it answered {:?}, which {} doesn't take", o.decision, h.on.name()))
    } else {
        o
    };
    if o.decision == Decision::Error { o.instead(h) } else { o }
}

thread_local! {
    static GUARDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether this thread is inside `Agent::apply`, whose panics are caught
/// (for panic hooks that stop the process on uncaught panics).
pub fn in_guarded_apply() -> bool {
    GUARDED.with(|g| g.get() > 0)
}

/// How a tool call is carried out.
enum Dispatch {
    /// Run this call (for call_tool: the call of the tool it names).
    Run(ToolCall),
    /// A lazy tool that isn't loaded: load it and show the schema instead.
    Load(String),
    Fail(String),
}

/// Resolves `call_tool` (and direct calls of lazy tools) to what runs. Pure:
/// replicas resolve the same calls the same way.
fn translate(spec: &Spec, loaded: &BTreeSet<String>, call: &ToolCall) -> Dispatch {
    let canonical = |n: &str| -> String {
        if spec.tools.iter().any(|t| t.name == n) {
            return n.to_string();
        }
        spec.tools.iter().find(|t| t.name.replace('.', "__") == n).map(|t| t.name.clone()).unwrap_or_else(|| n.to_string())
    };
    let lazy_or_run = |name: String, arguments: String, id: &str| {
        if spec.lazy.contains(&name) && !loaded.contains(&name) {
            Dispatch::Load(name)
        } else {
            Dispatch::Run(ToolCall { id: id.to_string(), kind: call.kind.clone(), function: crate::chat::FunctionCall { name, arguments } })
        }
    };
    if call.function.name != CALL_TOOL {
        return lazy_or_run(canonical(&call.function.name), call.function.arguments.clone(), &call.id);
    }
    #[derive(Deserialize)]
    struct Args {
        name: String,
        #[serde(default)]
        arguments: Value,
    }
    let a: Args = match serde_json::from_str(&call.function.arguments) {
        Ok(a) => a,
        Err(e) => return Dispatch::Fail(format!("error: bad arguments: {e}")),
    };
    let name = canonical(&a.name);
    if !spec.tools.iter().any(|t| t.name == name) || name == CALL_TOOL || name == LOAD_TOOLS {
        return Dispatch::Fail(format!("error: there's no tool {:?} to call (load_tools lists them)", a.name));
    }
    // Arguments may come as an object or as a JSON string holding one.
    let args = match a.arguments {
        Value::Null => "{}".to_string(),
        Value::String(s) => s,
        v => v.to_string(),
    };
    lazy_or_run(name, args, &call.id)
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

/// A tool result cut to its first `over` characters (on a line's end when
/// one is near), with a note saying how to search the rest; the whole is kept
/// in `full`.
pub fn cut_result(call_id: &str, content: String, over: usize) -> Message {
    let total = content.chars().count();
    let end = content.char_indices().nth(over).map_or(content.len(), |(i, _)| i);
    let head = &content[..end];
    // Back to the last line break in the last fifth, so lines stay whole.
    let head = match head.rfind('\n') {
        Some(i) if i >= end - end / 5 => &head[..i],
        _ => head,
    };
    let lines = content.lines().count();
    let shown = format!(
        "{head}\n[cut: this result is {total} characters ({lines} lines); you see the first {}. grep_result(call: {call_id:?}, pattern: …) searches all of it]",
        head.chars().count()
    );
    Message { full: Some(content), ..Message::tool(call_id, shown) }
}

