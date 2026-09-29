use super::*;
use crate::chat::{Role, ToolCallDelta};
use uuid::Uuid;

fn spec() -> Spec {
    Spec { ty: "t@h".into(), parent: None, budget: Budget::default(), approve: vec![] }
}

/// Applies events in order and records them so replay can be checked.
struct H {
    a: Agent,
    log: Vec<Event>,
}

impl H {
    fn new(spec: Spec) -> Self {
        Self { a: Agent::new(Uuid::from_u128(1), spec), log: vec![] }
    }
    fn ev(&mut self, e: Event) -> Vec<Effect> {
        self.log.push(e.clone());
        self.a.apply(&e)
    }
    fn user(&mut self, s: &str) -> Vec<Effect> {
        self.ev(Event::Inbox { from: Addr::root(), content: s.into(), reply: false })
    }
    fn text(&mut self, s: &str) -> Vec<Effect> {
        self.ev(Event::LlmDelta { delta: Delta { content: Some(s.into()), ..Default::default() } })
    }
    fn call(&mut self, idx: usize, id: &str, name: &str, args: &str) -> Vec<Effect> {
        self.ev(Event::LlmDelta {
            delta: Delta {
                tool_calls: vec![ToolCallDelta {
                    index: idx,
                    id: Some(id.into()),
                    name: Some(name.into()),
                    arguments: Some(args.into()),
                }],
                ..Default::default()
            },
        })
    }
    fn done(&mut self) -> Vec<Effect> {
        self.ev(Event::LlmDone)
    }
    fn result(&mut self, id: &str, s: &str) -> Vec<Effect> {
        self.ev(Event::ToolResult { call_id: id.into(), content: s.into(), is_error: false })
    }
    fn pause(&mut self, mode: PauseMode) -> Vec<Effect> {
        self.ev(Event::PauseRequested { mode })
    }
    fn resume(&mut self) -> Vec<Effect> {
        self.ev(Event::Resumed)
    }
    /// Crash + restart: replay the log from scratch, then the hub logs `Recovered`.
    fn crash(&mut self) -> Vec<Effect> {
        let a = Agent::replay(self.a.id, self.a.spec.clone(), &self.log);
        assert_eq!(a, self.a, "replay must reproduce the live state");
        self.a = a;
        self.ev(Event::Recovered)
    }
    fn contents(&self) -> Vec<(Role, String)> {
        self.a.messages.iter().map(|m| (m.role, m.content.clone().unwrap_or_default())).collect()
    }
}

fn report(to: Vec<Addr>, status: Status, content: &str) -> Effect {
    Effect::Report { to, status, content: content.into() }
}

fn tool_effect(fx: &[Effect]) -> ToolCall {
    match fx {
        [Effect::CallTool { call, .. }] => call.clone(),
        other => panic!("expected one CallTool, got {other:?}"),
    }
}

#[test]
fn message_starts_turn_and_answer_ends_it() {
    let mut h = H::new(spec());
    assert_eq!(h.user("hi"), vec![Effect::CallLlm]);
    assert_eq!(h.a.phase, Phase::Thinking { running: true });
    assert_eq!(h.a.llm_messages(), vec![Message::user("hi")]);
    h.text("hel");
    h.text("lo");
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "hello")]);
    assert_eq!(h.a.phase, Phase::Idle);
    assert_eq!(h.contents(), vec![(Role::User, "hi".into()), (Role::Assistant, "hello".into())]);
    assert!(h.a.acc.is_empty());
}

#[test]
fn tool_calls_run_sequentially() {
    let mut h = H::new(spec());
    h.user("do it");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    let c = tool_effect(&h.done());
    assert_eq!((c.id.as_str(), c.function.name.as_str()), ("c1", "a"));
    let c = tool_effect(&h.result("c1", "r1"));
    assert_eq!(c.id, "c2");
    assert_eq!(h.result("c2", "r2"), vec![Effect::CallLlm]);
    let tool_msgs: Vec<_> = h.a.messages.iter().filter_map(|m| m.tool_call_id.clone()).collect();
    assert_eq!(tool_msgs, ["c1", "c2"]);
}

#[test]
fn result_for_wrong_call_is_ignored() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.done();
    assert!(h.result("zzz", "r").is_empty());
    assert!(h.a.inflight());
}

#[test]
fn inbox_while_busy_is_injected_at_next_llm_call() {
    let mut h = H::new(spec());
    h.user("first");
    h.call(0, "c1", "a", "{}");
    h.done();
    assert!(h.user("second").is_empty());
    assert_eq!(h.result("c1", "r"), vec![Effect::CallLlm]);
    assert_eq!(h.contents().last().unwrap(), &(Role::User, "second".into()));
}

#[test]
fn inbox_during_final_answer_starts_new_turn() {
    let mut h = H::new(spec());
    h.user("first");
    h.user("second");
    h.text("answer");
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "answer"), Effect::CallLlm]);
}

#[test]
fn agent_messages_are_prefixed_and_replied_to() {
    let mut h = H::new(spec());
    let other = Addr::Agent(Uuid::from_u128(9));
    h.ev(Event::Inbox { from: other.clone(), content: "ping".into(), reply: false });
    assert!(h.contents()[0].1.starts_with(&format!("[message from {other}]")));
    h.text("pong");
    assert_eq!(h.done(), vec![report(vec![other], Status::Idle, "pong")]);
}

#[test]
fn replies_wake_but_are_not_owed_an_answer() {
    let b = Addr::Agent(Uuid::from_u128(9));
    let mut h = H::new(spec());
    h.user("ask b");
    h.text("asked");
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "asked")]);
    assert_eq!(h.ev(Event::Inbox { from: b.clone(), content: "answer".into(), reply: true }), vec![Effect::CallLlm]);
    assert_eq!(h.contents().last().unwrap().1, format!("[reply from {b}]\nanswer"));
    h.text("b says answer");
    // Not back to b (no ping-pong) but to whoever asked last.
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "b says answer")]);
}

#[test]
fn child_report_turn_answers_previous_askers() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.ev(Event::Inbox { from: Addr::Client("c".into()), content: "start".into(), reply: false });
    h.text("started");
    h.done();
    h.ev(child_report(kid(1), "result"));
    h.text("final");
    assert_eq!(h.done(), vec![report(vec![Addr::Client("c".into())], Status::Idle, "final")]);
}

#[test]
fn report_goes_to_parent_and_senders() {
    let parent = Uuid::from_u128(7);
    let mut h = H::new(Spec { parent: Some(parent), ..spec() });
    h.ev(Event::Inbox { from: Addr::Agent(parent), content: "task".into(), reply: false });
    h.ev(Event::Inbox { from: Addr::Client("s".into()), content: "also".into(), reply: false });
    h.text("ok");
    // The client's message arrived mid-turn, so it starts the next turn.
    assert_eq!(h.done(), vec![report(vec![Addr::Agent(parent)], Status::Idle, "ok"), Effect::CallLlm]);
    h.text("ok2");
    let Effect::Report { to, .. } = &h.done()[0] else { panic!() };
    assert_eq!(to, &vec![Addr::Client("s".into()), Addr::Agent(parent)]);
}

// ---------- pause modes ----------

#[test]
fn hard_pause_aborts_and_keeps_partial() {
    let mut h = H::new(spec());
    h.user("write");
    h.text("The quick ");
    h.call(0, "c1", "a", "{\"x");
    assert_eq!(h.pause(PauseMode::Hard), vec![Effect::AbortInflight]);
    assert!(!h.a.is_paused(), "still in flight until the abort lands");
    h.ev(Event::LlmAborted);
    assert!(h.a.is_paused());
    assert!(h.a.acc.tool_calls.is_empty(), "half tool calls are dropped");
    assert_eq!(h.a.llm_messages().last().unwrap(), &Message::assistant("The quick "));
    assert!(h.user("more").is_empty(), "paused agents only queue");
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
    h.text("brown fox");
    let fx = h.done();
    assert_eq!(fx[0], report(vec![Addr::root()], Status::Idle, "The quick brown fox"));
    // Queued message was injected before the continuation call, so no new turn.
    assert_eq!(fx.len(), 1);
}

#[test]
fn hard_pause_while_idle_needs_no_abort() {
    let mut h = H::new(spec());
    assert!(h.pause(PauseMode::Hard).is_empty());
    assert!(h.a.is_paused());
    assert!(h.user("hi").is_empty());
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

#[test]
fn hard_pause_aborts_tool_call() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.done();
    assert_eq!(h.pause(PauseMode::Hard), vec![Effect::AbortInflight]);
    assert!(h.ev(Event::ToolAborted { call_id: "c1".into() }).is_empty());
    assert!(h.a.is_paused());
    assert_eq!(h.a.messages.last().unwrap().content.as_deref(), Some("[aborted before completion]"));
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

#[test]
fn quick_pause_finishes_stream_but_starts_no_tool() {
    let mut h = H::new(spec());
    h.user("x");
    assert!(h.pause(PauseMode::Quick).is_empty());
    h.call(0, "c1", "a", "{}");
    assert!(h.done().is_empty());
    assert!(h.a.is_paused());
    let c = tool_effect(&h.resume());
    assert_eq!(c.id, "c1");
}

#[test]
fn quick_pause_finishes_tool_but_no_next_call() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    h.done();
    h.pause(PauseMode::Quick);
    assert!(h.result("c1", "r").is_empty(), "c2 must not start");
    assert!(h.a.is_paused());
    assert_eq!(tool_effect(&h.resume()).id, "c2");
}

#[test]
fn resume_while_still_inflight_does_not_duplicate() {
    let mut h = H::new(spec());
    h.user("x");
    h.pause(PauseMode::Quick);
    assert!(h.resume().is_empty(), "the stream is still running");
    h.text("a");
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "a")]);
}

#[test]
fn safe_pause_finishes_the_turn() {
    let mut h = H::new(spec());
    h.user("x");
    h.pause(PauseMode::Safe);
    h.call(0, "c1", "a", "{}");
    assert_eq!(tool_effect(&h.done()).id, "c1");
    assert_eq!(h.result("c1", "r"), vec![Effect::CallLlm]);
    assert!(!h.a.is_paused());
    h.user("queued");
    h.text("final");
    assert_eq!(h.done(), vec![report(vec![Addr::root()], Status::Idle, "final")], "no new turn while paused");
    assert!(h.a.is_paused());
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

#[test]
fn pause_escalates_never_downgrades() {
    let mut h = H::new(spec());
    h.user("x");
    h.pause(PauseMode::Safe);
    assert_eq!(h.pause(PauseMode::Hard), vec![Effect::AbortInflight]);
    h.pause(PauseMode::Quick);
    assert_eq!(h.a.pause, Some(PauseMode::Hard));
}

// ---------- crash recovery ----------

#[test]
fn crash_while_streaming_continues_partial() {
    let mut h = H::new(spec());
    h.user("x");
    h.text("half");
    h.call(0, "c1", "a", "{\"");
    assert_eq!(h.crash(), vec![Effect::CallLlm]);
    assert!(h.a.acc.tool_calls.is_empty());
    assert_eq!(h.a.llm_messages().last().unwrap(), &Message::assistant("half"));
}

#[test]
fn crash_while_tool_running_retries_it() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.done();
    match h.crash().as_slice() {
        [Effect::CallTool { call, retry: true }] => assert_eq!(call.id, "c1"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn crash_while_paused_stays_paused() {
    let mut h = H::new(spec());
    h.user("x");
    h.pause(PauseMode::Quick);
    assert!(h.crash().is_empty());
    assert!(h.a.is_paused(), "the in-flight call died with the process");
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

#[test]
fn crash_while_idle_does_nothing() {
    let mut h = H::new(spec());
    h.user("x");
    h.text("y");
    h.done();
    assert!(h.crash().is_empty());
}

#[test]
fn crash_while_awaiting_approval_asks_again() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "rm", "{}");
    h.done();
    assert!(matches!(h.crash().as_slice(), [Effect::RequestApproval { .. }]));
}

// ---------- approval ----------

#[test]
fn approval_granted_runs_tool() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "rm", "{}");
    assert!(matches!(h.done().as_slice(), [Effect::RequestApproval { call }] if call.id == "c1"));
    assert!(!h.a.inflight());
    let fx = h.ev(Event::Approval { call_id: "c1".into(), approved: true });
    assert!(matches!(fx.as_slice(), [Effect::CallTool { retry: false, .. }]));
}

#[test]
fn approval_while_paused_runs_on_resume() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "rm", "{}");
    h.done();
    h.pause(PauseMode::Quick);
    assert!(h.ev(Event::Approval { call_id: "c1".into(), approved: true }).is_empty());
    assert!(matches!(h.resume().as_slice(), [Effect::CallTool { retry: false, .. }]));
}

#[test]
fn approval_denied_tells_model() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "rm", "{}");
    h.done();
    assert_eq!(h.ev(Event::Approval { call_id: "c1".into(), approved: false }), vec![Effect::CallLlm]);
    assert_eq!(h.a.messages.last().unwrap().content.as_deref(), Some("[denied by user]"));
}

#[test]
fn approval_for_other_call_is_ignored() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "rm", "{}");
    h.done();
    assert!(h.ev(Event::Approval { call_id: "c9".into(), approved: true }).is_empty());
}

// ---------- children ----------

fn kid(n: u128) -> AgentId {
    Uuid::from_u128(100 + n)
}

fn child_report(id: AgentId, s: &str) -> Event {
    Event::ChildReport { id, status: Status::Idle, content: s.into() }
}

#[test]
fn wait_for_blocks_until_all_children_report() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.ev(Event::ChildSpawned { id: kid(2), reserved: 0 });
    h.user("x");
    h.call(0, "w", WAIT_FOR, &format!(r#"{{"ids":["{}","agent:{}"]}}"#, kid(1), kid(2)));
    assert!(h.done().is_empty());
    assert!(h.ev(child_report(kid(1), "one")).is_empty());
    assert_eq!(h.ev(child_report(kid(2), "two")), vec![Effect::CallLlm]);
    let out: Value = serde_json::from_str(h.a.messages.last().unwrap().content.as_deref().unwrap()).unwrap();
    assert_eq!(out[kid(1).to_string()][0]["content"], "one");
    assert_eq!(out[kid(2).to_string()][0]["status"], "idle");
    assert!(h.a.children.values().all(Vec::is_empty), "reports are consumed");
}

#[test]
fn wait_for_already_reported_child_returns_immediately() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.user("x");
    h.ev(child_report(kid(1), "early"));
    h.call(0, "w", WAIT_FOR, &format!(r#"{{"ids":["{}"]}}"#, kid(1)));
    assert_eq!(h.done(), vec![Effect::CallLlm]);
}

#[test]
fn wait_for_unknown_child_is_a_tool_error() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "w", WAIT_FOR, &format!(r#"{{"ids":["{}"]}}"#, kid(5)));
    assert_eq!(h.done(), vec![Effect::CallLlm]);
    assert!(h.a.messages.last().unwrap().content.as_deref().unwrap().contains("not a child"));
}

#[test]
fn wait_for_bad_args_is_a_tool_error() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "w", WAIT_FOR, "{nope");
    assert_eq!(h.done(), vec![Effect::CallLlm]);
    assert!(h.a.messages.last().unwrap().content.as_deref().unwrap().starts_with("error: bad arguments"));
}

#[test]
fn child_report_wakes_idle_parent() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    assert_eq!(h.ev(child_report(kid(1), "done!")), vec![Effect::CallLlm]);
    assert!(h.contents()[0].1.starts_with(&format!("[report from agent:{} (idle)]", kid(1))));
}

#[test]
fn child_report_while_busy_waits_for_next_call() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.done();
    assert!(h.ev(child_report(kid(1), "r")).is_empty());
    assert_eq!(h.result("c1", "ok"), vec![Effect::CallLlm]);
    assert!(h.contents().last().unwrap().1.contains("[report from"));
}

#[test]
fn crash_while_waiting_for_children_keeps_waiting() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.user("x");
    h.call(0, "w", WAIT_FOR, &format!(r#"{{"ids":["{}"]}}"#, kid(1)));
    h.done();
    assert!(h.crash().is_empty());
    assert_eq!(h.ev(child_report(kid(1), "r")), vec![Effect::CallLlm]);
}

// ---------- failure, cancel, budget ----------

#[test]
fn cancel_aborts_and_reports() {
    let parent = Uuid::from_u128(7);
    let mut h = H::new(Spec { parent: Some(parent), ..spec() });
    h.user("x");
    let fx = h.ev(Event::Cancelled);
    assert_eq!(fx[0], Effect::AbortInflight);
    assert_eq!(fx[1], report(vec![Addr::root(), Addr::Agent(parent)], Status::Cancelled, "[cancelled]"));
    assert!(h.done().is_empty(), "late completion is dropped");
    assert_eq!(h.a.phase, Phase::Cancelled);
    assert!(h.user("hello?").is_empty());
    assert!(h.resume().is_empty());
    assert!(h.ev(Event::Cancelled).is_empty(), "cancel is idempotent");
}

#[test]
fn llm_failure_then_resume_continues_partial() {
    let mut h = H::new(spec());
    h.user("x");
    h.text("par");
    let fx = h.ev(Event::LlmFailed { error: "boom".into() });
    assert_eq!(fx, vec![report(vec![Addr::root()], Status::Failed, "boom")]);
    assert!(matches!(h.a.phase, Phase::Failed { .. }));
    assert!(h.user("still there?").is_empty());
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
    h.text("tial");
    let Effect::Report { content, to, .. } = &h.done()[0] else { panic!() };
    assert_eq!(content, "partial");
    assert_eq!(to, &vec![Addr::root()], "the answer still reaches whoever asked");
}

#[test]
fn token_budget_fails_the_agent() {
    let mut h = H::new(Spec { budget: Budget { max_tokens: Some(10), ..Default::default() }, ..spec() });
    h.user("x");
    h.ev(Event::LlmDelta {
        delta: Delta { usage: Some(Usage { prompt_tokens: 8, completion_tokens: 4 }), ..Default::default() },
    });
    h.call(0, "c1", "a", "{}");
    h.done();
    assert_eq!(h.a.usage.total(), 12);
    let fx = h.result("c1", "r");
    assert!(
        matches!(fx.as_slice(), [Effect::Report { status: Status::Failed, content, .. }] if content.contains("budget"))
    );
}

#[test]
fn tokens_reserved_for_children_count_against_budget() {
    let mut h = H::new(Spec { budget: Budget { max_tokens: Some(100), ..Default::default() }, ..spec() });
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 100 });
    assert_eq!(h.a.remaining_tokens(), Some(0));
    assert!(matches!(h.user("x").as_slice(), [Effect::Report { status: Status::Failed, .. }]));
}

#[test]
fn unlimited_budget_has_no_remaining() {
    assert_eq!(H::new(spec()).a.remaining_tokens(), None);
}

#[test]
fn events_roundtrip_through_json() {
    let evs = vec![
        Event::Inbox { from: Addr::Client("c".into()), content: "x".into(), reply: true },
        Event::LlmDelta { delta: Delta { content: Some("a".into()), ..Default::default() } },
        Event::LlmDone,
        Event::ToolResult { call_id: "c".into(), content: "r".into(), is_error: true },
        Event::ChildReport { id: kid(1), status: Status::Failed, content: "e".into() },
        Event::PauseRequested { mode: PauseMode::Quick },
        Event::Cancelled,
    ];
    for e in evs {
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&j).unwrap(), e, "{j}");
    }
    assert_eq!(serde_json::to_value(Event::Resumed).unwrap(), json!({"type":"resumed"}));
}

#[test]
fn replay_of_long_session_matches_live_state() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.user("a");
    h.call(0, "c1", "rm", "{}");
    h.done();
    h.ev(Event::Approval { call_id: "c1".into(), approved: true });
    h.pause(PauseMode::Quick);
    h.result("c1", "ok");
    h.user("b");
    h.resume();
    h.text("t");
    h.pause(PauseMode::Hard);
    h.ev(Event::LlmAborted);
    h.ev(child_report(kid(1), "x"));
    h.resume();
    h.text("u");
    h.done();
    h.crash(); // asserts replay == live
    h.crash(); // including the Recovered event
}
