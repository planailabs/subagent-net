//! The flight recorder: a run's event log read as steps, and two runs laid
//! side by side to show where they parted ways.
//!
//! Pure: no I/O, no clock. A step is the unit a person reads and a diff
//! compares — a streamed answer is one step, not two hundred chunks — and its
//! `key` says *where in the run* it happened, content aside, so two runs of
//! the same task line up even though tool call ids and token counts differ.
//! `digest` says *what* happened: two steps with one key and different digests
//! are the same structural point reached differently, which is exactly the
//! question a replay is asked.
//!
//! `Agent::apply` stays the source of truth for state. This module only reads
//! the log, so replaying can never change an agent.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::Event;
use crate::chat::Accumulator;

/// What happened, for a timeline's glyphs and a divergence's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A message arriving (the task itself is the first one).
    Input,
    /// One streamed model answer, its deltas folded together.
    Stream,
    StreamAborted,
    StreamFailed,
    ToolResult,
    ToolAborted,
    /// A user approved or denied a call.
    Approval,
    ChildSpawned,
    ChildReport,
    Pause,
    Resume,
    Cancel,
    Superseded,
    /// The node died (or was reassigned): what was in flight is gone.
    Recovered,
    ToolsLoaded,
    ToolsChanged,
    Compacted,
    CompactFailed,
    CompactRequested,
    HookDone,
}

impl StepKind {
    /// What a divergence of this kind is about.
    fn cause(self) -> Cause {
        match self {
            StepKind::Input => Cause::Input,
            StepKind::Stream | StepKind::StreamAborted | StepKind::StreamFailed => Cause::Model,
            StepKind::ToolResult | StepKind::ToolAborted | StepKind::Approval => Cause::Tool,
            StepKind::Compacted | StepKind::CompactFailed | StepKind::CompactRequested => Cause::Context,
            _ => Cause::Control,
        }
    }
}

/// One readable unit of a run: a streamed answer, a tool result, a pause…
/// Several log entries can be one step (a stream's deltas), and one entry is
/// never split.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    /// First log entry of the step.
    pub seq: u64,
    /// Last one (equal to `seq` unless a stream's deltas were folded).
    pub last: u64,
    /// When it started (milliseconds since the epoch), when the caller had
    /// timestamps for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,
    pub kind: StepKind,
    /// Where in the run this is, content aside: what two runs align on.
    pub key: String,
    /// One line for a timeline.
    pub label: String,
    /// The start of what was said, returned or summarised (for a tooltip).
    pub preview: String,
    /// Equal digests mean the same thing happened, content included.
    pub digest: String,
}

/// How much of a preview is kept: enough to recognise a step, not a transcript.
const PREVIEW: usize = 240;

/// Steps of a log whose first event is `first` (its seq number).
pub fn steps<'a>(first: u64, events: impl IntoIterator<Item = &'a Event>) -> Vec<Step> {
    timed_steps(events.into_iter().enumerate().map(|(n, e)| (first + n as u64, None, e)))
}

/// Steps of a log with timestamps: `(seq, at_ms, event)`.
pub fn timed_steps<'a>(events: impl IntoIterator<Item = (u64, Option<i64>, &'a Event)>) -> Vec<Step> {
    let mut f = Folder { out: vec![], acc: Accumulator::default(), stream: None, names: HashMap::new() };
    for (seq, at, ev) in events {
        f.push(seq, ev, at);
    }
    f.finish()
}

/// Folds events into steps, keeping the one piece of state a step needs from
/// earlier ones: the stream that proposed each tool call (results carry only
/// the id, which differs between runs).
struct Folder {
    out: Vec<Step>,
    acc: Accumulator,
    /// The stream being folded: first seq, last seq, start time.
    stream: Option<(u64, u64, Option<i64>)>,
    names: HashMap<String, String>,
}

impl Folder {
    fn push(&mut self, seq: u64, ev: &Event, at: Option<i64>) {
        if let Event::LlmDelta { delta } = ev {
            self.acc.push(delta);
            self.stream = Some(match self.stream {
                Some((from, _, sat)) => (from, seq, sat),
                None => (seq, seq, at),
            });
            return;
        }
        self.flush();
        if let Some((kind, key, label, preview, dg)) = self.event(ev) {
            self.out.push(Step { seq, last: seq, at_ms: at, kind, key, label, preview, digest: dg });
        }
    }

    /// Emits the stream that is open, if any: one step whatever its length.
    fn flush(&mut self) {
        let Some((from, to, at)) = self.stream.take() else { return };
        let calls: Vec<String> =
            self.acc.tool_calls.iter().map(|c| c.name.clone().unwrap_or_else(|| "?".into())).collect();
        for c in &self.acc.tool_calls {
            if let (Some(id), Some(name)) = (&c.id, &c.name) {
                self.names.insert(id.clone(), name.clone());
            }
        }
        let mut parts: Vec<String> = vec!["llm".into(), self.acc.content.clone()];
        for c in &self.acc.tool_calls {
            parts.push(format!("{}:{}", c.name.as_deref().unwrap_or("?"), c.arguments.clone().unwrap_or_default()));
        }
        let chars = self.acc.content.chars().count();
        let label = match calls.is_empty() {
            true => format!("model: {chars} chars"),
            false => format!("model: {chars} chars, calls {}", calls.join(", ")),
        };
        let preview = match self.acc.content.is_empty() {
            true => calls.join(", "),
            false => head(&self.acc.content),
        };
        self.out.push(Step {
            seq: from,
            last: to,
            at_ms: at,
            kind: StepKind::Stream,
            key: "llm".into(),
            label,
            preview,
            digest: digest(&parts),
        });
        self.acc = Accumulator::default();
    }

    fn finish(mut self) -> Vec<Step> {
        self.flush();
        self.out
    }

    /// The step a non-delta event makes (or none: `LlmDone` closes a stream
    /// that is already a step).
    fn event(&self, ev: &Event) -> Option<(StepKind, String, String, String, String)> {
        let name = |call: &str| self.names.get(call).cloned().unwrap_or_else(|| "tool".into());
        Some(match ev {
            Event::LlmDelta { .. } => unreachable!("the delta branch handles it"),
            Event::LlmDone => return None,
            Event::LlmAborted => {
                (StepKind::StreamAborted, "aborted".into(), "aborted".into(), head("aborted"), digest(&["aborted"]))
            }
            Event::LlmFailed { error } => (
                StepKind::StreamFailed,
                "llm_failed".into(),
                format!("model failed: {}", head_n(error, 80)),
                head(error),
                digest(&["failed", error]),
            ),
            Event::Inbox { from, content, .. } => (
                StepKind::Input,
                format!("input:{from}"),
                format!("from {from}: {}", head_n(content, 80)),
                head(content),
                digest(&["inbox", content]),
            ),
            Event::ToolResult { call_id, content, is_error } => {
                let n = name(call_id);
                let verdict = if *is_error { "error" } else { "ok" };
                let label = format!("{n} → {verdict} ({} chars)", content.chars().count());
                (StepKind::ToolResult, format!("tool:{n}"), label, head(content), digest(&["tool", content, verdict]))
            }
            Event::ToolAborted { call_id } => {
                let n = name(call_id);
                (
                    StepKind::ToolAborted,
                    format!("aborted:{n}"),
                    format!("{n} aborted"),
                    head("aborted"),
                    digest(&["aborted", &n]),
                )
            }
            Event::Approval { call_id, approved, .. } => {
                let n = name(call_id);
                let verdict = if *approved { "approved" } else { "denied" };
                (
                    StepKind::Approval,
                    format!("approval:{n}"),
                    format!("{n} {verdict}"),
                    head(verdict),
                    digest(&["approval", verdict]),
                )
            }
            Event::ChildSpawned { id, reserved } => (
                StepKind::ChildSpawned,
                "child_spawned".into(),
                format!("spawned child {}", short(&id.to_string())),
                head(&format!("reserved {reserved} tokens")),
                digest(&["spawned", &id.to_string(), &reserved.to_string()]),
            ),
            Event::ChildReport { id, status, content } => {
                let st = json_name(status);
                (
                    StepKind::ChildReport,
                    format!("report:{st}"),
                    format!("child {} reported {st}", short(&id.to_string())),
                    head(content),
                    digest(&["report", &id.to_string(), &st, content]),
                )
            }
            Event::PauseRequested { mode } => {
                let m = json_name(mode);
                (StepKind::Pause, format!("pause:{m}"), format!("pause ({m})"), head(&m), digest(&["pause", &m]))
            }
            Event::Resumed => {
                (StepKind::Resume, "resume".into(), "resumed".into(), head("resumed"), digest(&["resumed"]))
            }
            Event::Cancelled => {
                (StepKind::Cancel, "cancel".into(), "cancelled".into(), head("cancelled"), digest(&["cancelled"]))
            }
            Event::Superseded { by } => (
                StepKind::Superseded,
                "superseded".into(),
                format!("moved to agent {}", short(&by.to_string())),
                head("superseded"),
                digest(&["superseded", &by.to_string()]),
            ),
            Event::Recovered => (
                StepKind::Recovered,
                "recovered".into(),
                "recovered (its node died)".into(),
                head("recovered"),
                digest(&["recovered"]),
            ),
            Event::ToolsLoaded { names } => (
                StepKind::ToolsLoaded,
                "tools_loaded".into(),
                format!("loaded {} tools", names.len()),
                head(&names.join(", ")),
                digest(&["tools_loaded", &names.join(",")]),
            ),
            Event::ToolsChanged { tools, added, .. } => {
                let offered: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
                (
                    StepKind::ToolsChanged,
                    "tools_changed".into(),
                    format!("tools changed (+{}: {})", added.len(), added.join(", ")),
                    head(&offered.join(", ")),
                    digest(&["tools_changed", &offered.join(",")]),
                )
            }
            Event::Compacted { upto, summary, .. } => (
                StepKind::Compacted,
                "compacted".into(),
                format!("compacted to {upto} messages"),
                head(summary),
                digest(&["compacted", summary]),
            ),
            Event::CompactFailed { error } => (
                StepKind::CompactFailed,
                "compact_failed".into(),
                "compaction failed".into(),
                head(error),
                digest(&["compact_failed", error]),
            ),
            Event::CompactRequested => (
                StepKind::CompactRequested,
                "compact_requested".into(),
                "compaction asked for".into(),
                head(""),
                digest(&["compact_requested"]),
            ),
            Event::HookDone { id, outcome } => {
                let decision = json_name(&outcome.decision);
                let reason = outcome.reason.as_deref().unwrap_or_default();
                (
                    StepKind::HookDone,
                    format!("hook:{decision}"),
                    format!("hook {id}: {decision}"),
                    head(reason),
                    digest(&["hook", id, &decision, reason]),
                )
            }
        })
    }
}

/// How a row of the side-by-side view came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The same step, the same content.
    Same,
    /// The same structural step (same key), different content.
    Changed,
    OnlyA,
    OnlyB,
}

/// One row: A's step, B's step, or both when they align.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Row {
    pub status: Status,
    /// Index into A's steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<usize>,
    /// Index into B's steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<usize>,
}

/// Why two runs stopped being the same run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// A different agent type, model, tools or budget (the caller knows).
    Spec,
    /// A different message came in.
    Input,
    /// The model answered differently from the same point on.
    Model,
    /// A tool returned something else (or failed where it worked).
    Tool,
    /// It compacted differently: what the model was sent changed.
    Context,
    /// Pausing, resuming, cancelling, a node dying.
    Control,
    /// One run has a step the other does not (it went on, or it stopped).
    Extra,
    Unknown,
}

impl Cause {
    /// A sentence for a human, before the caller adds what it knows.
    pub fn why(self) -> &'static str {
        match self {
            Cause::Spec => "the runs were built differently (type, model, tools or budget)",
            Cause::Input => "a different message came in",
            Cause::Model => "the model answered differently from here",
            Cause::Tool => "a tool returned something else",
            Cause::Context => "compaction differed, so the model was sent a different context",
            Cause::Control => "the run was controlled differently (pause, cancel, a node dying)",
            Cause::Extra => "one run has steps the other does not",
            Cause::Unknown => "the runs differ",
        }
    }
}

/// The first place the runs stopped matching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Divergence {
    /// Index into `Diff::rows`.
    pub row: usize,
    pub cause: Cause,
    /// What happened, in a sentence.
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<Step>,
}

/// Two runs, aligned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Diff {
    pub rows: Vec<Row>,
    /// Rows that matched, content and all.
    pub same: usize,
    /// Rows that matched structurally but differ in content.
    pub changed: usize,
    pub only_a: usize,
    pub only_b: usize,
    /// How much of the two runs is identical, content included: 0.0..=1.0.
    pub similarity: f64,
    /// How much lines up structurally (a changed step still counts): 0.0..=1.0.
    pub alignment: f64,
    /// None when the runs are the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<Divergence>,
}

/// Aligns two step lists and reports the first place they part.
pub fn diff(a: &[Step], b: &[Step]) -> Diff {
    // Identical heads and tails need no alignment: this is also the common
    // case (a fork compared with its original shares all of its prefix).
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head].key == b[head].key && a[head].digest == b[head].digest {
        head += 1;
    }
    let mut tail = 0;
    while tail < a.len() - head
        && tail < b.len() - head
        && a[a.len() - 1 - tail].key == b[b.len() - 1 - tail].key
        && a[a.len() - 1 - tail].digest == b[b.len() - 1 - tail].digest
    {
        tail += 1;
    }
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);

    let mut rows: Vec<Row> = (0..head).map(|i| Row { status: Status::Same, a: Some(i), b: Some(i) }).collect();

    let keys_a: Vec<&str> = am.iter().map(|s| s.key.as_str()).collect();
    let keys_b: Vec<&str> = bm.iter().map(|s| s.key.as_str()).collect();
    let pairs = lcs(&keys_a, &keys_b);

    let mut i = 0;
    let mut j = 0;
    for (pa, pb) in pairs {
        while i < pa {
            rows.push(Row { status: Status::OnlyA, a: Some(head + i), b: None });
            i += 1;
        }
        while j < pb {
            rows.push(Row { status: Status::OnlyB, a: None, b: Some(head + j) });
            j += 1;
        }
        let status = if am[pa].digest == bm[pb].digest { Status::Same } else { Status::Changed };
        rows.push(Row { status, a: Some(head + pa), b: Some(head + pb) });
        i = pa + 1;
        j = pb + 1;
    }
    while i < am.len() {
        rows.push(Row { status: Status::OnlyA, a: Some(head + i), b: None });
        i += 1;
    }
    while j < bm.len() {
        rows.push(Row { status: Status::OnlyB, a: None, b: Some(head + j) });
        j += 1;
    }
    for k in 0..tail {
        let (ia, ib) = (a.len() - tail + k, b.len() - tail + k);
        rows.push(Row { status: Status::Same, a: Some(ia), b: Some(ib) });
    }

    let same = rows.iter().filter(|r| r.status == Status::Same).count();
    let changed = rows.iter().filter(|r| r.status == Status::Changed).count();
    let only_a = rows.iter().filter(|r| r.status == Status::OnlyA).count();
    let only_b = rows.iter().filter(|r| r.status == Status::OnlyB).count();
    let n = a.len() + b.len();
    let frac = |k: usize| if n == 0 { 1.0 } else { 2.0 * k as f64 / n as f64 };

    let first = rows.iter().enumerate().find(|(_, r)| r.status != Status::Same).map(|(n, r)| {
        let sa = r.a.map(|k| a[k].clone());
        let sb = r.b.map(|k| b[k].clone());
        let (cause, note) = match r.status {
            Status::Changed => {
                let kind = sa.as_ref().map(|s| s.kind).unwrap_or(StepKind::Input);
                let what = sa.as_ref().map(|s| s.label.clone()).unwrap_or_default();
                (kind.cause(), format!("both sides did '{what}', but the outcome differs"))
            }
            Status::OnlyA => (
                Cause::Extra,
                format!("A did '{}' and B has no such step", sa.as_ref().map(|s| s.label.as_str()).unwrap_or("")),
            ),
            Status::OnlyB => (
                Cause::Extra,
                format!("B did '{}' and A has no such step", sb.as_ref().map(|s| s.label.as_str()).unwrap_or("")),
            ),
            Status::Same => (Cause::Unknown, String::new()),
        };
        Divergence { row: n, cause, note, a: sa, b: sb }
    });

    Diff { rows, same, changed, only_a, only_b, similarity: frac(same), alignment: frac(same + changed), first }
}

/// The longest common subsequence of two key lists, as index pairs.
///
/// The table is O(n·m) `u32`, so pair of very long logs (over four million
/// cells, say 2000×2000 steps after their identical head and tail are cut)
/// falls back to [`greedy`]: exactness matters less than the answer arriving.
fn lcs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return vec![];
    }
    if n.saturating_mul(m) > 4_000_000 {
        return greedy(a, b);
    }
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] =
                if a[i] == b[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
        }
    }
    let mut pairs = vec![];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Matches keys within a small window, skipping the ones that don't come back:
/// for logs too long for [`lcs`]'s table.
fn greedy(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    const LOOK: usize = 32;
    let (mut i, mut j) = (0usize, 0usize);
    let mut pairs = vec![];
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
            continue;
        }
        let ahead_a = (1..=LOOK).find(|&d| i + d < a.len() && a[i + d] == b[j]);
        let ahead_b = (1..=LOOK).find(|&d| j + d < b.len() && b[j + d] == a[i]);
        match (ahead_a, ahead_b) {
            (Some(x), Some(y)) if y < x => j += y,
            (Some(x), _) => i += x,
            (None, Some(y)) => j += y,
            (None, None) => i += 1,
        }
    }
    pairs
}

/// The first `n` characters, on one line.
fn head(s: &str) -> String {
    head_n(s, PREVIEW)
}

fn head_n(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect::<String>().replace(['\r', '\n'], " ");
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// The first eight characters of an id.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A `serde`-serialised enum's variant name (snake_case).
fn json_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn digest<P: AsRef<str>>(parts: &[P]) -> String {
    let mut h = DefaultHasher::new();
    for p in parts {
        p.as_ref().hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::Addr;
    use crate::chat::{Delta, ToolCallDelta};

    fn say(text: &str) -> Vec<Event> {
        let mut ev = vec![];
        for chunk in text.as_bytes().chunks(5) {
            ev.push(Event::LlmDelta {
                delta: Delta { content: Some(String::from_utf8_lossy(chunk).into()), ..Default::default() },
            });
        }
        ev.push(Event::LlmDone);
        ev
    }

    fn call(id: &str, name: &str, args: &str) -> Event {
        Event::LlmDelta {
            delta: Delta {
                tool_calls: vec![ToolCallDelta {
                    index: 0,
                    id: Some(id.into()),
                    name: Some(name.into()),
                    arguments: Some(args.into()),
                }],
                ..Default::default()
            },
        }
    }

    fn task(text: &str) -> Event {
        Event::Inbox { from: Addr::User("root".into()), content: text.into(), reply: false }
    }

    fn run(answer: &str, result: &str) -> Vec<Event> {
        let mut ev = vec![task("fix the bug")];
        ev.push(call("c1", "read_file", r#"{"path":"x"}"#));
        ev.extend(say(answer));
        ev.push(Event::ToolResult { call_id: "c1".into(), content: result.into(), is_error: false });
        ev.extend(say("done"));
        ev
    }

    #[test]
    fn a_stream_is_one_step() {
        let ev = say("hello there, this is a long streamed answer");
        let s = steps(0, &ev);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, StepKind::Stream);
        assert_eq!(s[0].seq, 0);
        // The step ends at the last delta; `LlmDone` only closes it.
        assert_eq!(s[0].last, ev.len() as u64 - 2);
        assert!(s[0].label.contains("43 chars"), "{}", s[0].label);
    }

    #[test]
    fn tool_calls_get_names_on_their_results() {
        let mut ev = vec![task("t")];
        ev.push(call("c1", "read_file", "{}"));
        ev.push(Event::LlmDone);
        ev.push(Event::ToolResult { call_id: "c1".into(), content: "ok".into(), is_error: false });
        let s = steps(0, &ev);
        assert_eq!(s.len(), 3, "task, stream, result");
        assert_eq!(s[2].key, "tool:read_file");
        assert_eq!(s[2].kind, StepKind::ToolResult);
        assert_eq!(s[2].label, "read_file → ok (2 chars)");
    }

    #[test]
    fn the_same_run_is_identical() {
        let a = steps(0, &run("answer", "result"));
        let b = steps(0, &run("answer", "result"));
        let d = diff(&a, &b);
        assert!(d.rows.iter().all(|r| r.status == Status::Same));
        assert_eq!(d.similarity, 1.0);
        assert_eq!(d.first, None);
    }

    #[test]
    fn a_different_tool_result_is_the_tool() {
        let a = steps(0, &run("answer", "result"));
        let b = steps(0, &run("answer", "different result"));
        let d = diff(&a, &b);
        assert_eq!(d.first.expect("must diverge").cause, Cause::Tool);
        assert_eq!(d.changed, 1);
        assert_eq!(d.same, a.len() - 1);
    }

    #[test]
    fn a_different_answer_is_the_model() {
        let a = steps(0, &run("answer", "result"));
        let b = steps(0, &run("another answer", "result"));
        let d = diff(&a, &b);
        assert_eq!(d.first.expect("must diverge").cause, Cause::Model);
    }

    #[test]
    fn a_different_task_is_the_input() {
        let a = steps(0, &run("answer", "result"));
        let mut ev = run("answer", "result");
        ev[0] = task("do something else");
        let d = diff(&a, &steps(0, &ev));
        assert_eq!(d.first.expect("must diverge").cause, Cause::Input);
    }

    #[test]
    fn an_extra_step_is_extra() {
        let a = steps(0, &run("answer", "result"));
        let mut more = run("answer", "result");
        more.push(Event::ToolResult { call_id: "c1".into(), content: "again".into(), is_error: false });
        let b = steps(0, &more);
        let d = diff(&a, &b);
        assert_eq!(d.only_b, 1);
        assert_eq!(d.first.expect("must diverge").cause, Cause::Extra);
        assert_eq!(d.same, a.len(), "every step of A still lines up");
    }

    #[test]
    fn a_truncated_run_is_the_other_runs_prefix() {
        // A fork's first N steps against the original's: everything it has
        // is the original's prefix.
        let a = steps(0, &run("answer", "result"));
        let d = diff(&a, &a[..2]);
        assert_eq!(d.only_a, a.len() - 2);
        assert_eq!(d.only_b, 0);
        assert_eq!(d.changed, 0);
    }

    #[test]
    fn empty_runs_are_the_same() {
        let d = diff(&[], &[]);
        assert_eq!(d.similarity, 1.0);
        assert!(d.rows.is_empty());
    }

    #[test]
    fn a_cancelled_run_diverges_at_the_control_step() {
        let a = steps(0, &run("answer", "result"));
        let mut ev = run("answer", "result");
        ev.push(Event::Cancelled);
        ev.push(Event::ToolResult { call_id: "c1".into(), content: "late".into(), is_error: false });
        let d = diff(&a, &steps(0, &ev));
        let first = d.first.expect("must diverge");
        assert_eq!(first.cause, Cause::Extra);
        assert_eq!(first.b.map(|s| s.kind), Some(StepKind::Cancel));
    }

    #[test]
    fn long_logs_fall_back_to_greedy_alignment() {
        // Different content everywhere: no identical head or tail to cut, so
        // the table would be 2100×2100 and the fallback runs.
        let mk = |n: usize, salt: &str| -> Vec<Step> {
            (0..n)
                .map(|i| Step {
                    seq: i as u64,
                    last: i as u64,
                    at_ms: None,
                    kind: StepKind::ToolResult,
                    key: format!("tool:{i}"),
                    label: String::new(),
                    preview: String::new(),
                    digest: format!("{i}-{salt}"),
                })
                .collect()
        };
        let (a, b) = (mk(2100, "a"), mk(2100, "b"));
        let d = diff(&a, &b);
        assert_eq!(d.changed, 2100);
        assert_eq!(d.alignment, 1.0);
        assert_eq!(d.similarity, 0.0);
    }
}
