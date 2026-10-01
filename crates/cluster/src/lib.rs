//! Cluster files: HCL documents describing principals, agent types, MCP
//! servers, mixtures, residents, senses and switchboard routes. Parsing,
//! validation, type identity (hashes), per-node views and diffs. No I/O.

mod time;

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subnet_core::agent::Budget;

pub use time::{Dur, Rate};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("{file}: {msg}")]
    Parse { file: String, msg: String },
    #[error("{0}")]
    Invalid(String),
}

fn invalid<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error::Invalid(m.into()))
}

/// A single `x { … }` block or several of them.
fn one_or_many<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        Many(Vec<T>),
        One(T),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::Many(v) => v,
        OneOrMany::One(t) => vec![t],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrincipalDef {
    pub role: Role,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeDef {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Agents this node runs at once.
    #[serde(default = "default_capacity")]
    pub capacity: u32,
}

fn default_capacity() -> u32 {
    16
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// Env var on the node holding the API key.
    #[serde(default)]
    pub env: Option<String>,
    pub base_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Executor {
    #[serde(default)]
    pub internal: bool,
    /// External executor speaking the runner protocol on stdio.
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

impl Executor {
    pub fn is_external(&self) -> bool {
        self.command.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDef {
    #[serde(default)]
    pub description: String,
    pub credential: Credential,
    pub model: String,
    #[serde(default)]
    pub params: Map<String, Value>,
    /// Endpoint can continue a trailing assistant message.
    #[serde(default)]
    pub prefill: bool,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub executor: Executor,
    /// Nodes that run this type.
    pub nodes: Vec<String>,
    /// Mixtures (or agent types) agents of this type may spawn.
    #[serde(default)]
    pub spawns: Vec<String>,
    #[serde(default)]
    pub budget: Budget,
    /// Tools needing approval: built-ins or `<mcp>.<tool>`.
    #[serde(default)]
    pub approve: Vec<String>,
    /// Conversation compaction (on unless `enabled = false`).
    #[serde(default, skip_serializing_if = "CompactDef::is_default")]
    pub compact: CompactDef,
    /// The model sees images (`vision = {}` for the defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<subnet_core::agent::Vision>,
}

/// `compact { ... }` of an agent: when a model call's context reaches
/// `at_tokens`, all but the task and the last `keep` messages are replaced
/// by a summary the model writes. External executors never compact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct CompactDef {
    pub enabled: bool,
    pub at_tokens: u64,
    pub keep: usize,
}

impl Default for CompactDef {
    fn default() -> Self {
        Self { enabled: true, at_tokens: 96_000, keep: 8 }
    }
}

impl CompactDef {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// What agents of a type get, if anything.
    pub fn spec(&self, executor: &Executor) -> Option<subnet_core::agent::Compact> {
        (self.enabled && !executor.is_external()).then(|| subnet_core::agent::Compact { at_tokens: self.at_tokens, keep: self.keep })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpCredential {
    /// HTTP header to send, e.g. `Authorization`.
    pub header: String,
    /// Env var on the node holding the value.
    pub env: String,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpDef {
    #[serde(default)]
    pub description: String,
    /// Stdio server command.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Environment for the stdio server; `$VAR` reads the node's env.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Streamable-HTTP server URL.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub credential: Option<McpCredential>,
    pub nodes: Vec<String>,
    /// Tools safe to re-run after a crash interrupted them.
    #[serde(default)]
    pub idempotent: Vec<String>,
    /// Agents see only this server's tool names until they load them
    /// (`load_tools`, or a mixture's router). `false`: full schemas always.
    #[serde(default = "yes")]
    pub lazy: bool,
    /// One server per tenant (a stdio command): `${TENANT}` in `env` is the
    /// tenant of the agent calling. Started on first use, stopped when idle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub per_tenant: bool,
    /// The tenant of the instance that lists the tools and serves agents
    /// without a tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tenant: Option<String>,
}

fn yes() -> bool {
    true
}

/// Pre-loads lazy tools that match a message (embedding similarity between
/// the message and the tools' names and descriptions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouterDef {
    /// At most this many tools per message.
    #[serde(default = "RouterDef::default_top_k")]
    pub top_k: usize,
    /// Cosine similarity a tool needs. e5 squeezes scores into a narrow high
    /// band (unrelated ≈ 0.75, related ≈ 0.8), so this is a coarse filter:
    /// a wrong pre-load costs a few schema tokens, a missed one is still a
    /// `load_tools` away.
    #[serde(default = "RouterDef::default_min_score")]
    pub min_score: f32,
}

impl RouterDef {
    fn default_top_k() -> usize {
        3
    }
    fn default_min_score() -> f32 {
        0.78
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MixtureDef {
    #[serde(default)]
    pub description: String,
    pub agent: String,
    #[serde(default)]
    pub mcp: Vec<String>,
    /// Mailboxes agents of this mixture may read.
    #[serde(default)]
    pub mailboxes: Vec<String>,
    /// Pre-load matching lazy tools for every message sent to its agents.
    #[serde(default)]
    pub router: Option<RouterDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResidentDef {
    /// Mixture (or agent type) to run.
    pub mixture: String,
    /// First message when the resident is created.
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    #[serde(default)]
    pub every: Option<Dur>,
    #[serde(default)]
    pub cron: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileWatch {
    pub path: String,
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// With `exec`: the binary format the command writes (the sense publishes
    /// a stream). Alone: the name of a sense whose stream to subscribe to.
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub webhook: Option<Webhook>,
    #[serde(default)]
    pub timer: Option<Timer>,
    #[serde(default)]
    pub file: Option<FileWatch>,
}

/// What a source is, after validation.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind<'a> {
    Exec(&'a [String]),
    /// `exec` whose stdout is a binary stream of this format.
    ExecStream(&'a [String], &'a str),
    Subscribe(&'a str),
    Webhook(&'a Webhook),
    Timer(&'a Timer),
    File(&'a FileWatch),
}

impl Source {
    pub fn kind(&self) -> Result<SourceKind<'_>, String> {
        let n = [self.exec.is_some(), self.webhook.is_some(), self.timer.is_some(), self.file.is_some()]
            .iter()
            .filter(|b| **b)
            .count();
        match (n, &self.exec, &self.stream) {
            (1, Some(e), Some(f)) => Ok(SourceKind::ExecStream(e, f)),
            (1, Some(e), None) => Ok(SourceKind::Exec(e)),
            (0, None, Some(s)) => Ok(SourceKind::Subscribe(s)),
            (1, None, None) => Ok(if let Some(w) = &self.webhook {
                SourceKind::Webhook(w)
            } else if let Some(t) = &self.timer {
                SourceKind::Timer(t)
            } else {
                SourceKind::File(self.file.as_ref().unwrap())
            }),
            (1, None, Some(_)) => Err("`stream` goes with `exec` (publish) or alone (subscribe)".into()),
            _ => Err("source needs exactly one of exec, stream, webhook, timer, file".into()),
        }
    }

    /// Stream format if this source publishes a stream.
    pub fn publishes(&self) -> Option<&str> {
        match self.kind() {
            Ok(SourceKind::ExecStream(_, f)) => Some(f),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// CEL → bool; sees `event` and `prev`.
    #[serde(default)]
    pub filter: Option<String>,
    /// CEL → new event data.
    #[serde(default)]
    pub map: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SenseDef {
    #[serde(default)]
    pub description: String,
    pub node: String,
    pub source: Source,
    #[serde(default)]
    pub stage: IndexMap<String, Stage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub window: Dur,
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dedupe {
    /// CEL → key.
    pub key: String,
    pub within: Dur,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpDeliver {
    pub server: String,
    pub tool: String,
    /// CEL → argument object.
    #[serde(default)]
    pub args: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deliver {
    #[serde(default)]
    pub spawn: Option<String>,
    /// CEL → string; with `spawn`.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub send: Option<String>,
    #[serde(default)]
    pub mailbox: Option<String>,
    #[serde(default)]
    pub mcp: Option<McpDeliver>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteDef {
    pub from: String,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub map: Option<String>,
    #[serde(default)]
    pub throttle: Option<Rate>,
    #[serde(default)]
    pub debounce: Option<Dur>,
    #[serde(default)]
    pub batch: Option<Batch>,
    #[serde(default)]
    pub dedupe: Option<Dedupe>,
    #[serde(default)]
    pub max_active: Option<u32>,
    #[serde(deserialize_with = "one_or_many")]
    pub deliver: Vec<Deliver>,
}

/// A whole cluster description.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cluster {
    #[serde(default, rename = "user")]
    pub users: IndexMap<String, PrincipalDef>,
    #[serde(default, rename = "client")]
    pub clients: IndexMap<String, PrincipalDef>,
    #[serde(default, rename = "node")]
    pub nodes: IndexMap<String, NodeDef>,
    #[serde(default, rename = "agent")]
    pub agents: IndexMap<String, AgentDef>,
    #[serde(default, rename = "mcp")]
    pub mcps: IndexMap<String, McpDef>,
    #[serde(default, rename = "mixture")]
    pub mixtures: IndexMap<String, MixtureDef>,
    #[serde(default, rename = "resident")]
    pub residents: IndexMap<String, ResidentDef>,
    #[serde(default, rename = "sense")]
    pub senses: IndexMap<String, SenseDef>,
    #[serde(default, rename = "route")]
    pub routes: IndexMap<String, RouteDef>,
}

const KINDS: &[&str] = &["user", "client", "node", "agent", "mcp", "mixture", "resident", "sense", "route"];

impl Cluster {
    /// Parses and validates one or more files (`(name, text)`); their blocks
    /// are merged and a name may be declared only once per kind.
    pub fn parse(files: &[(&str, &str)]) -> Result<Self, Error> {
        let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut merged = Cluster::default();
        for (name, text) in files {
            let perr = |msg: String| Error::Parse { file: name.to_string(), msg };
            let body: hcl::Body = hcl::from_str(text).map_err(|e| perr(e.to_string()))?;
            for s in body.iter() {
                match s {
                    hcl::Structure::Attribute(a) => {
                        return Err(perr(format!("unexpected top-level attribute {:?}", a.key.as_str())));
                    }
                    hcl::Structure::Block(b) => {
                        let kind = b.identifier.as_str().to_string();
                        if !KINDS.contains(&kind.as_str()) {
                            return Err(perr(format!("unknown block {kind:?} (expected one of {})", KINDS.join(", "))));
                        }
                        let [label] = b.labels.as_slice() else {
                            return Err(perr(format!("{kind} block needs exactly one name label")));
                        };
                        let label = label.as_str().to_string();
                        if let Some(prev) = seen.insert((kind.clone(), label.clone()), name.to_string()) {
                            return Err(perr(format!("{kind} {label:?} already declared in {prev}")));
                        }
                    }
                }
            }
            let c: Cluster = hcl::from_str(text).map_err(|e| perr(e.to_string()))?;
            merged.users.extend(c.users);
            merged.clients.extend(c.clients);
            merged.nodes.extend(c.nodes);
            merged.agents.extend(c.agents);
            merged.mcps.extend(c.mcps);
            merged.mixtures.extend(c.mixtures);
            merged.residents.extend(c.residents);
            merged.senses.extend(c.senses);
            merged.routes.extend(c.routes);
        }
        merged.validate()?;
        Ok(merged)
    }

    /// Checks names, references and exclusive options.
    pub fn validate(&self) -> Result<(), Error> {
        let names = self
            .users
            .keys()
            .chain(self.clients.keys())
            .chain(self.nodes.keys())
            .chain(self.agents.keys())
            .chain(self.mcps.keys())
            .chain(self.mixtures.keys())
            .chain(self.residents.keys())
            .chain(self.senses.keys())
            .chain(self.routes.keys());
        for n in names {
            if n.is_empty() || n.contains(['@', ':', '.', '/', ' ']) {
                return invalid(format!("name {n:?} may not be empty or contain @ : . / or spaces"));
            }
        }
        if let Some(k) = self.users.keys().find(|k| self.clients.contains_key(*k)) {
            return invalid(format!("{k:?} is both a user and a client"));
        }
        let node = |ctx: &str, n: &str| -> Result<(), Error> {
            if self.nodes.contains_key(n) { Ok(()) } else { invalid(format!("{ctx}: unknown node {n:?}")) }
        };
        let spawnable = |n: &str| self.mixtures.contains_key(n) || self.agents.contains_key(n);
        for (name, a) in &self.agents {
            let ctx = format!("agent {name:?}");
            if a.nodes.is_empty() {
                return invalid(format!("{ctx}: nodes must not be empty"));
            }
            for n in &a.nodes {
                node(&ctx, n)?;
            }
            for s in &a.spawns {
                if !spawnable(s) {
                    return invalid(format!("{ctx}: spawns unknown mixture or agent {s:?}"));
                }
            }
            if a.compact.enabled && (a.compact.at_tokens == 0 || a.compact.keep == 0) {
                return invalid(format!("{ctx}: compact needs at_tokens and keep above 0"));
            }
            if a.executor.internal && a.executor.command.is_some() {
                return invalid(format!("{ctx}: executor is either internal or a command"));
            }
            if a.executor.command.as_ref().is_some_and(Vec::is_empty) {
                return invalid(format!("{ctx}: executor command is empty"));
            }
        }
        for (name, m) in &self.mcps {
            let ctx = format!("mcp {name:?}");
            match (&m.command, &m.url) {
                (Some(c), None) if !c.is_empty() => {}
                (None, Some(_)) => {}
                _ => return invalid(format!("{ctx}: needs exactly one of command or url")),
            }
            if m.credential.is_some() && m.url.is_none() {
                return invalid(format!("{ctx}: credential is for url servers; use env for stdio"));
            }
            if m.per_tenant && m.command.is_none() {
                return invalid(format!("{ctx}: per_tenant needs a command (each tenant gets its own process)"));
            }
            if m.per_tenant && m.default_tenant.is_none() {
                return invalid(format!("{ctx}: per_tenant needs a default_tenant (its instance lists the tools and serves agents without a tenant)"));
            }
            if m.default_tenant.is_some() && !m.per_tenant {
                return invalid(format!("{ctx}: default_tenant only applies with per_tenant"));
            }
            if m.nodes.is_empty() {
                return invalid(format!("{ctx}: nodes must not be empty"));
            }
            for n in &m.nodes {
                node(&ctx, n)?;
            }
        }
        for (name, x) in &self.mixtures {
            let ctx = format!("mixture {name:?}");
            if !self.agents.contains_key(&x.agent) {
                return invalid(format!("{ctx}: unknown agent {:?}", x.agent));
            }
            if self.agents.contains_key(name) {
                return invalid(format!("{ctx}: an agent type has the same name"));
            }
            for m in &x.mcp {
                if !self.mcps.contains_key(m) {
                    return invalid(format!("{ctx}: unknown mcp {m:?}"));
                }
            }
            if let Some(r) = &x.router
                && (r.top_k == 0 || !(-1.0..=1.0).contains(&r.min_score))
            {
                return invalid(format!("{ctx}: router needs top_k >= 1 and min_score in -1..1"));
            }
        }
        for (name, r) in &self.residents {
            if !spawnable(&r.mixture) {
                return invalid(format!("resident {name:?}: unknown mixture or agent {:?}", r.mixture));
            }
        }
        for (name, s) in &self.senses {
            let ctx = format!("sense {name:?}");
            node(&ctx, &s.node)?;
            match s.source.kind().map_err(|e| Error::Invalid(format!("{ctx}: {e}")))? {
                SourceKind::Subscribe(from) => match self.senses.get(from) {
                    Some(src) if src.source.publishes().is_some() => {}
                    Some(_) => return invalid(format!("{ctx}: sense {from:?} publishes no stream")),
                    None => return invalid(format!("{ctx}: unknown stream {from:?}")),
                },
                SourceKind::Timer(t) => match (&t.every, &t.cron) {
                    (Some(_), None) => {}
                    (None, Some(c)) => {
                        c.parse::<croner::Cron>().map_err(|e| Error::Invalid(format!("{ctx}: cron {c:?}: {e}")))?;
                    }
                    _ => return invalid(format!("{ctx}: timer needs exactly one of every or cron")),
                },
                SourceKind::Webhook(w) if !w.path.starts_with('/') => {
                    return invalid(format!("{ctx}: webhook path must start with /"));
                }
                _ => {}
            }
            if matches!(s.source.kind(), Ok(SourceKind::Subscribe(_)))
                && !s.stage.values().next().is_some_and(|st| st.exec.is_some())
            {
                return invalid(format!("{ctx}: a stream subscriber's first stage must be exec (it gets raw bytes)"));
            }
            if s.source.publishes().is_some() && !s.stage.is_empty() {
                return invalid(format!("{ctx}: a sense publishing a stream has no stages; subscribe to it instead"));
            }
            for (sn, st) in &s.stage {
                let n = [st.exec.is_some(), st.filter.is_some(), st.map.is_some()].iter().filter(|b| **b).count();
                if n != 1 {
                    return invalid(format!("{ctx} stage {sn:?}: needs exactly one of exec, filter, map"));
                }
            }
        }
        for (name, r) in &self.routes {
            let ctx = format!("route {name:?}");
            match self.senses.get(&r.from) {
                None => return invalid(format!("{ctx}: unknown sense {:?}", r.from)),
                Some(s) if s.source.publishes().is_some() => {
                    return invalid(format!("{ctx}: sense {:?} publishes a stream, not events", r.from));
                }
                _ => {}
            }
            if r.deliver.is_empty() {
                return invalid(format!("{ctx}: needs at least one deliver"));
            }
            for d in &r.deliver {
                let n = [d.spawn.is_some(), d.send.is_some(), d.mailbox.is_some(), d.mcp.is_some()]
                    .iter()
                    .filter(|b| **b)
                    .count();
                if n != 1 {
                    return invalid(format!("{ctx}: each deliver needs exactly one of spawn, send, mailbox, mcp"));
                }
                if d.prompt.is_some() && d.spawn.is_none() {
                    return invalid(format!("{ctx}: prompt goes with spawn"));
                }
                if let Some(s) = &d.spawn
                    && !spawnable(s)
                {
                    return invalid(format!("{ctx}: unknown mixture or agent {s:?}"));
                }
                if let Some(s) = &d.send
                    && !self.residents.contains_key(s)
                {
                    return invalid(format!("{ctx}: send needs a resident, {s:?} isn't one"));
                }
                if let Some(m) = &d.mcp
                    && !self.mcps.contains_key(&m.server)
                {
                    return invalid(format!("{ctx}: unknown mcp {:?}", m.server));
                }
            }
            if r.max_active.is_some() && !r.deliver.iter().any(|d| d.spawn.is_some()) {
                return invalid(format!("{ctx}: max_active only applies to spawn deliveries"));
            }
        }
        Ok(())
    }

    /// Identity of an agent type: `name@hash` over everything but placement.
    pub fn agent_id(&self, name: &str) -> Option<String> {
        let a = self.agents.get(name)?;
        let mut v = serde_json::to_value(a).unwrap();
        v.as_object_mut().unwrap().remove("nodes");
        Some(format!("{name}@{}", hash(&v)))
    }

    /// Identity of an MCP type.
    pub fn mcp_id(&self, name: &str) -> Option<String> {
        let m = self.mcps.get(name)?;
        let mut v = serde_json::to_value(m).unwrap();
        v.as_object_mut().unwrap().remove("nodes");
        Some(format!("{name}@{}", hash(&v)))
    }

    /// What one node runs.
    pub fn node_config(&self, node: &str) -> Option<NodeConfig> {
        let def = self.nodes.get(node)?;
        let agents = self
            .agents
            .iter()
            .filter(|(_, a)| a.nodes.iter().any(|n| n == node))
            .map(|(n, a)| NodeAgent { id: self.agent_id(n).unwrap(), name: n.clone(), def: a.clone() })
            .collect();
        let mcps = self
            .mcps
            .iter()
            .filter(|(_, m)| m.nodes.iter().any(|n| n == node))
            .map(|(n, m)| NodeMcp { id: self.mcp_id(n).unwrap(), name: n.clone(), def: m.clone() })
            .collect();
        let senses: IndexMap<String, SenseDef> =
            self.senses.iter().filter(|(_, s)| s.node == node).map(|(n, s)| (n.clone(), s.clone())).collect();
        // Streams crossing nodes go through the hub's relay.
        let subscribe_to = |s: &SenseDef| match s.source.kind() {
            Ok(SourceKind::Subscribe(from)) => Some(from.to_string()),
            _ => None,
        };
        let mut relay_out: Vec<String> = senses
            .keys()
            .filter(|n| self.senses.values().any(|o| o.node != node && subscribe_to(o).as_deref() == Some(n.as_str())))
            .cloned()
            .collect();
        let mut relay_in: Vec<String> = senses
            .values()
            .filter_map(subscribe_to)
            .filter(|from| self.senses.get(from).is_some_and(|p| p.node != node))
            .collect();
        relay_out.sort();
        relay_in.sort();
        relay_in.dedup();
        Some(NodeConfig { node: node.to_string(), capacity: def.capacity, agents, mcps, senses, relay_out, relay_in })
    }

    /// Changes from `self` to `new`, per block.
    pub fn diff(&self, new: &Cluster) -> Vec<Change> {
        let a = serde_json::to_value(self).unwrap();
        let b = serde_json::to_value(new).unwrap();
        let mut out = vec![];
        for kind in KINDS {
            // Serialized field names are the block kinds.
            let empty = Map::new();
            let old = a[*kind].as_object().unwrap_or(&empty);
            let new = b[*kind].as_object().unwrap_or(&empty);
            let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
            for k in keys {
                let action = match (old.get(k), new.get(k)) {
                    (None, Some(_)) => Action::Added,
                    (Some(_), None) => Action::Removed,
                    (Some(x), Some(y)) if x != y => Action::Changed,
                    _ => continue,
                };
                out.push(Change { kind: kind.to_string(), name: k.clone(), action });
            }
        }
        out
    }
}

fn hash(v: &Value) -> String {
    let bytes = serde_json::to_vec(v).unwrap();
    Sha256::digest(&bytes).iter().take(6).map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeAgent {
    pub name: String,
    /// `name@hash`
    pub id: String,
    pub def: AgentDef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeMcp {
    pub name: String,
    pub id: String,
    pub def: McpDef,
}

/// The part of a cluster one node runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeConfig {
    pub node: String,
    pub capacity: u32,
    pub agents: Vec<NodeAgent>,
    pub mcps: Vec<NodeMcp>,
    pub senses: IndexMap<String, SenseDef>,
    /// Streams published here that senses on other nodes subscribe to.
    #[serde(default)]
    pub relay_out: Vec<String>,
    /// Streams subscribed here that other nodes publish.
    #[serde(default)]
    pub relay_in: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    pub kind: String,
    pub name: String,
    pub action: Action,
}

#[cfg(test)]
mod tests;
