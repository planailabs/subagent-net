//! The switchboard's logic: CEL expressions and per-route flow control.
//! Time is passed in (milliseconds), so everything here is deterministic and
//! free of I/O; the hub owns the clock, the timers and the deliveries.

use std::collections::{HashMap, VecDeque};

use cel_interpreter::{Context, Program};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subnet_cluster::{Cluster, Deliver, RouteDef};

/// A compiled CEL expression.
pub struct Expr {
    src: String,
    prog: Program,
}

impl std::fmt::Debug for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Expr({:?})", self.src)
    }
}

/// JSON → CEL. Integers become `int` (not `uint`), so `event.n * 2` works
/// the way people expect; other numbers become `double`.
fn to_cel(v: &Value) -> cel_interpreter::Value {
    use cel_interpreter::Value as C;
    use cel_interpreter::objects::{Key, Map};
    match v {
        Value::Null => C::Null,
        Value::Bool(b) => C::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => C::Int(i),
            None => C::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => C::String(s.clone().into()),
        Value::Array(a) => C::List(a.iter().map(to_cel).collect::<Vec<_>>().into()),
        Value::Object(o) => C::Map(Map {
            map: std::sync::Arc::new(o.iter().map(|(k, v)| (Key::String(k.clone().into()), to_cel(v))).collect()),
        }),
    }
}

/// The CEL library can panic on odd input; that must not take the hub down.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(format!("CEL {what}: invalid expression")))
}

impl Expr {
    pub fn compile(src: &str) -> Result<Self, String> {
        let prog = guarded(src, || Program::compile(src).map_err(|e| format!("CEL {src:?}: {e}")))?;
        Ok(Self { src: src.to_string(), prog })
    }

    pub fn eval(&self, vars: &[(&str, &Value)]) -> Result<Value, String> {
        let mut ctx = Context::default();
        for (k, v) in vars {
            ctx.add_variable_from_value(*k, to_cel(v));
        }
        let out = guarded(&self.src, || self.prog.execute(&ctx).map_err(|e| format!("CEL {:?}: {e}", self.src)))?;
        out.json().map_err(|e| format!("CEL {:?}: result is not JSON: {e:?}", self.src))
    }

    pub fn eval_bool(&self, vars: &[(&str, &Value)]) -> Result<bool, String> {
        match self.eval(vars)? {
            Value::Bool(b) => Ok(b),
            other => Err(format!("CEL {:?} must be true or false, got {other}", self.src)),
        }
    }
}

/// An event as routes see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenseEvent {
    pub id: String,
    pub sense: String,
    /// Unix milliseconds.
    pub at: u64,
    pub data: Value,
}

impl SenseEvent {
    /// CEL variables: `event` (the data), `sense`, `at`, `id`.
    fn vars(&self) -> [(&'static str, Value); 4] {
        [
            ("event", self.data.clone()),
            ("sense", json!(self.sense)),
            ("at", json!(self.at)),
            ("id", json!(self.id)),
        ]
    }
}

fn eval_with(e: &Expr, vars: &[(&'static str, Value)]) -> Result<Value, String> {
    let refs: Vec<(&str, &Value)> = vars.iter().map(|(k, v)| (*k, v)).collect();
    e.eval(&refs)
}

/// A route with its expressions compiled.
#[derive(Debug)]
pub struct Route {
    pub name: String,
    pub def: RouteDef,
    when: Option<Expr>,
    map: Option<Expr>,
    dedupe: Option<Expr>,
    /// Per deliver: prompt (spawn) or args (mcp) expression.
    exprs: Vec<Option<Expr>>,
}

/// Compiles every route of a cluster; the first bad expression is an error.
pub fn compile(c: &Cluster) -> Result<Vec<Route>, String> {
    c.routes.iter().map(|(n, r)| Route::compile(n, r).map_err(|e| format!("route {n:?}: {e}"))).collect()
}

/// A delivery ready to be performed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    /// The (mapped) event data, or the last one of a batch.
    pub event: Value,
    /// All events delivered together (one unless batched).
    pub batch: Vec<Value>,
    /// Ids of the source events.
    pub ids: Vec<String>,
}

/// What to do for one `deliver` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Spawn { mixture: String, prompt: String },
    Send { resident: String, content: String },
    Mailbox { name: String, content: String },
    Mcp { server: String, tool: String, args: Value },
    Freeze { hold: String },
    Release { hold: String },
}

impl Action {
    /// Freezing or releasing a hold: done before an event's other deliveries.
    pub fn controls(&self) -> Option<(&str, bool)> {
        match self {
            Action::Freeze { hold } => Some((hold, true)),
            Action::Release { hold } => Some((hold, false)),
            _ => None,
        }
    }
}

impl Route {
    pub fn compile(name: &str, def: &RouteDef) -> Result<Self, String> {
        let opt = |s: &Option<String>| s.as_deref().map(Expr::compile).transpose();
        let exprs = def
            .deliver
            .iter()
            .map(|d| match (&d.prompt, &d.mcp) {
                (Some(p), _) => Expr::compile(p).map(Some),
                (None, Some(m)) => m.args.as_deref().map(Expr::compile).transpose(),
                _ => Ok(None),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            name: name.to_string(),
            def: def.clone(),
            when: opt(&def.when)?,
            map: opt(&def.map)?,
            dedupe: def.dedupe.as_ref().map(|d| Expr::compile(&d.key)).transpose()?,
            exprs,
        })
    }

    /// The actions a payload turns into, one per `deliver`.
    pub fn actions(&self, p: &Payload) -> Result<Vec<Action>, String> {
        let vars = [("event", p.event.clone()), ("batch", Value::Array(p.batch.clone()))];
        let text = serde_json::to_string(if p.batch.len() == 1 { &p.event } else { &vars[1].1 }).unwrap();
        self.def
            .deliver
            .iter()
            .zip(&self.exprs)
            .map(|(d, e): (&Deliver, &Option<Expr>)| {
                let rendered = |e: &Option<Expr>| e.as_ref().map(|e| eval_with(e, &vars)).transpose();
                Ok(if let Some(m) = &d.spawn {
                    let prompt = match rendered(e)? {
                        Some(Value::String(s)) => s,
                        Some(other) => other.to_string(),
                        None => format!("Event from route {}:\n{text}", self.name),
                    };
                    Action::Spawn { mixture: m.clone(), prompt }
                } else if let Some(r) = &d.send {
                    Action::Send { resident: r.clone(), content: text.clone() }
                } else if let Some(n) = &d.mailbox {
                    Action::Mailbox { name: n.clone(), content: text.clone() }
                } else if let Some(h) = &d.freeze {
                    Action::Freeze { hold: h.clone() }
                } else if let Some(h) = &d.release {
                    Action::Release { hold: h.clone() }
                } else {
                    let m = d.mcp.as_ref().expect("validated: exactly one deliver kind");
                    let args = rendered(e)?.unwrap_or_else(|| json!({"event": p.event, "batch": p.batch}));
                    if !args.is_object() {
                        return Err(format!("mcp args must be an object, got {args}"));
                    }
                    Action::Mcp { server: m.server.clone(), tool: m.tool.clone(), args }
                })
            })
            .collect()
    }
}

/// Counters per route, for the switchboard view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Counters {
    /// Events seen from the route's sense.
    pub seen: u64,
    /// Dropped by `when`.
    pub filtered: u64,
    /// Dropped by `dedupe`.
    pub deduped: u64,
    /// Replaced by a later event within the debounce window.
    pub debounced: u64,
    /// Dropped by `throttle`.
    pub throttled: u64,
    /// Payloads delivered.
    pub delivered: u64,
    /// Expression errors.
    pub errors: u64,
}

/// Flow-control state of one route.
#[derive(Debug, Default)]
pub struct RouteState {
    pub counters: Counters,
    seen_keys: HashMap<String, u64>,
    debounce: Option<(Value, String, u64)>,
    batch: Option<(Vec<Value>, Vec<String>, u64)>,
    sent: VecDeque<u64>,
    pub last_error: Option<String>,
}

impl RouteState {
    /// Feeds one event; returns payloads ready now.
    pub fn offer(&mut self, r: &Route, ev: &SenseEvent, now: u64) -> Vec<Payload> {
        self.counters.seen += 1;
        let vars = ev.vars();
        if let Some(w) = &r.when {
            let refs: Vec<(&str, &Value)> = vars.iter().map(|(k, v)| (*k, v)).collect();
            match w.eval_bool(&refs) {
                Ok(true) => {}
                Ok(false) => {
                    self.counters.filtered += 1;
                    return vec![];
                }
                Err(e) => return self.error(e),
            }
        }
        let data = match &r.map {
            Some(m) => match eval_with(m, &vars) {
                Ok(v) => v,
                Err(e) => return self.error(e),
            },
            None => ev.data.clone(),
        };
        if let (Some(k), Some(d)) = (&r.dedupe, &r.def.dedupe) {
            let key = match eval_with(k, &[("event", data.clone())]) {
                Ok(Value::String(s)) => s,
                Ok(v) => v.to_string(),
                Err(e) => return self.error(e),
            };
            let within = d.within.0.as_millis() as u64;
            self.seen_keys.retain(|_, t| now.saturating_sub(*t) < within);
            if self.seen_keys.contains_key(&key) {
                self.counters.deduped += 1;
                return vec![];
            }
            self.seen_keys.insert(key, now);
        }
        if let Some(d) = r.def.debounce {
            if self.debounce.is_some() {
                self.counters.debounced += 1;
            }
            self.debounce = Some((data, ev.id.clone(), now + d.0.as_millis() as u64));
            return vec![];
        }
        self.after_debounce(r, data, ev.id.clone(), now)
    }

    fn error(&mut self, e: String) -> Vec<Payload> {
        self.counters.errors += 1;
        self.last_error = Some(e);
        vec![]
    }

    fn after_debounce(&mut self, r: &Route, data: Value, id: String, now: u64) -> Vec<Payload> {
        match &r.def.batch {
            Some(b) => {
                let (items, ids, _) =
                    self.batch.get_or_insert_with(|| (vec![], vec![], now + b.window.0.as_millis() as u64));
                items.push(data);
                ids.push(id);
                if b.max.is_some_and(|m| items.len() as u32 >= m) {
                    let (items, ids, _) = self.batch.take().unwrap();
                    return self.throttle(r, items, ids, now);
                }
                vec![]
            }
            None => self.throttle(r, vec![data], vec![id], now),
        }
    }

    fn throttle(&mut self, r: &Route, batch: Vec<Value>, ids: Vec<String>, now: u64) -> Vec<Payload> {
        if let Some(t) = r.def.throttle {
            let per = t.per.0.as_millis() as u64;
            while self.sent.front().is_some_and(|s| now.saturating_sub(*s) >= per) {
                self.sent.pop_front();
            }
            if self.sent.len() as u32 >= t.n {
                self.counters.throttled += 1;
                return vec![];
            }
            self.sent.push_back(now);
        }
        self.counters.delivered += 1;
        let event = batch.last().cloned().unwrap_or(Value::Null);
        vec![Payload { event, batch, ids }]
    }

    /// Fires due debounce/batch timers.
    pub fn tick(&mut self, r: &Route, now: u64) -> Vec<Payload> {
        let mut out = vec![];
        if self.debounce.as_ref().is_some_and(|(_, _, due)| *due <= now) {
            let (data, id, _) = self.debounce.take().unwrap();
            out.extend(self.after_debounce(r, data, id, now));
        }
        if self.batch.as_ref().is_some_and(|(_, _, due)| *due <= now) {
            let (items, ids, _) = self.batch.take().unwrap();
            out.extend(self.throttle(r, items, ids, now));
        }
        out
    }

    /// When `tick` should run next.
    pub fn next_due(&self) -> Option<u64> {
        [self.debounce.as_ref().map(|d| d.2), self.batch.as_ref().map(|b| b.2)].into_iter().flatten().min()
    }
}

/// A CEL stage of a sense (`filter` or `map`), which also sees `prev`, the
/// last event the stage let through.
pub struct Stage {
    kind: StageKind,
    prev: Value,
}

enum StageKind {
    Filter(Expr),
    Map(Expr),
}

impl Stage {
    pub fn filter(src: &str) -> Result<Self, String> {
        Ok(Self { kind: StageKind::Filter(Expr::compile(src)?), prev: Value::Null })
    }

    pub fn map(src: &str) -> Result<Self, String> {
        Ok(Self { kind: StageKind::Map(Expr::compile(src)?), prev: Value::Null })
    }

    /// The event data to pass on, or `None` to drop it.
    pub fn apply(&mut self, data: Value) -> Result<Option<Value>, String> {
        let out = match &self.kind {
            StageKind::Filter(e) => e.eval_bool(&[("event", &data), ("prev", &self.prev)])?.then_some(data),
            StageKind::Map(e) => Some(e.eval(&[("event", &data), ("prev", &self.prev)])?),
        };
        if let Some(v) = &out {
            self.prev = v.clone();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
