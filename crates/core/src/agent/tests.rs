use super::*;
use crate::chat::{Role, ToolCallDelta};
use uuid::Uuid;

fn spec() -> Spec {
    Spec::of_type("t@h")
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

fn tool_effects(fx: &[Effect]) -> Vec<(String, bool)> {
    fx.iter()
        .map(|e| match e {
            Effect::CallTool { call, retry } => (call.id.clone(), *retry),
            other => panic!("expected only CallTool, got {other:?}"),
        })
        .collect()
}

#[test]
fn tool_calls_run_in_parallel() {
    let mut h = H::new(spec());
    h.user("do it");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    assert_eq!(tool_effects(&h.done()), [("c1".to_string(), false), ("c2".to_string(), false)]);
    // Results in any order; the next LLM call waits for all of them.
    assert!(h.result("c2", "r2").is_empty());
    assert_eq!(h.result("c1", "r1"), vec![Effect::CallLlm]);
    let tool_msgs: Vec<_> = h.a.messages.iter().filter_map(|m| m.tool_call_id.clone()).collect();
    assert_eq!(tool_msgs, ["c2", "c1"], "recorded in the order they finished");
}

#[test]
fn duplicate_results_are_ignored() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    h.done();
    h.result("c1", "r");
    assert!(h.result("c1", "again").is_empty());
    assert_eq!(h.a.messages.iter().filter(|m| m.tool_call_id.as_deref() == Some("c1")).count(), 1);
}

#[test]
fn hard_pause_aborts_all_running_calls() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    h.done();
    assert_eq!(h.pause(PauseMode::Hard), vec![Effect::AbortInflight]);
    h.ev(Event::ToolAborted { call_id: "c1".into() });
    assert!(!h.a.is_paused(), "c2 is still running");
    h.ev(Event::ToolAborted { call_id: "c2".into() });
    assert!(h.a.is_paused());
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

#[test]
fn crash_restarts_every_running_call() {
    let mut h = H::new(spec());
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "b", "{}");
    h.done();
    h.result("c1", "ok");
    assert_eq!(tool_effects(&h.crash()), [("c2".to_string(), true)]);
}

#[test]
fn wait_for_runs_alongside_other_tools() {
    let mut h = H::new(spec());
    h.ev(Event::ChildSpawned { id: kid(1), reserved: 0 });
    h.user("x");
    h.call(0, "w", WAIT_FOR, &format!(r#"{{"ids":["{}"]}}"#, kid(1)));
    h.call(1, "c", "a", "{}");
    assert_eq!(tool_effects(&h.done()), [("c".to_string(), false)]);
    assert!(h.result("c", "ok").is_empty(), "still waiting for the child");
    assert_eq!(h.ev(child_report(kid(1), "r")), vec![Effect::CallLlm]);
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
fn quick_pause_finishes_running_calls_but_starts_none() {
    let mut h = H::new(Spec { approve: vec!["rm".into()], ..spec() });
    h.user("x");
    h.call(0, "c1", "a", "{}");
    h.call(1, "c2", "rm", "{}");
    let fx = h.done();
    assert!(matches!(fx.as_slice(), [Effect::CallTool { .. }, Effect::RequestApproval { .. }]), "{fx:?}");
    h.pause(PauseMode::Quick);
    assert!(h.ev(Event::Approval { call_id: "c2".into(), approved: true }).is_empty(), "approved but not started");
    assert!(h.result("c1", "r").is_empty());
    assert!(h.a.is_paused());
    assert_eq!(tool_effects(&h.resume()), [("c2".to_string(), false)]);
    assert_eq!(h.result("c2", "done"), vec![Effect::CallLlm]);
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
        delta: Delta { usage: Some(Usage { prompt_tokens: 8, completion_tokens: 4, ..Default::default() }), ..Default::default() },
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

fn tool(name: &str, description: &str) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), parameters: json!({"type":"object","properties":{}}) }
}

fn lazy_spec() -> Spec {
    Spec {
        tools: vec![tool("world.say", "Say it. Out loud."), tool("web.search", "Search the web."), tool("web.scrape", "Read a page.")],
        lazy: vec!["web.search".into(), "web.scrape".into()],
        ..spec()
    }
}

fn names(tools: &[ToolDef]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_str()).collect()
}

#[test]
fn the_offered_tools_never_change() {
    // The provider caches the tool list and the conversation after it: any
    // change to the tools would throw the whole cached conversation away.
    let mut h = H::new(lazy_spec());
    let before = h.a.offered_tools();
    assert_eq!(names(&before), ["world.say", LOAD_TOOLS, CALL_TOOL]);
    let load = &before[1].description;
    assert!(load.contains("- web.search: Search the web.") && load.contains("- web.scrape: Read a page."), "{load}");
    h.user("find it");
    h.call(0, "c1", LOAD_TOOLS, r#"{"names":["web.search","nope"]}"#);
    assert_eq!(h.done(), vec![Effect::CallLlm], "resolved by the state machine itself");
    let result = h.contents().last().unwrap().1.clone();
    assert!(result.starts_with("loaded web.search; call them with call_tool"), "{result}");
    assert!(result.contains(r#""name":"web.search""#) && result.contains(r#""parameters""#), "the schema is in the conversation: {result}");
    assert!(result.ends_with("unknown: nope"), "{result}");
    assert_eq!(h.a.offered_tools(), before);
    h.ev(Event::ToolsLoaded { names: vec!["web.scrape".into()] });
    assert_eq!(h.a.offered_tools(), before);
    h.crash();
}

#[test]
fn call_tool_runs_the_tool_it_names() {
    let mut h = H::new(lazy_spec());
    h.user("go");
    h.call(0, "c1", LOAD_TOOLS, r#"{"names":["web"]}"#);
    h.done();
    assert!(h.a.loaded.contains("web.search") && h.a.loaded.contains("web.scrape"), "a server loads all its tools");
    h.call(0, "c2", CALL_TOOL, r#"{"name":"web__search","arguments":{"q":"sei"}}"#);
    let call = tool_effect(&h.done());
    assert_eq!((call.id.as_str(), call.function.name.as_str(), call.function.arguments.as_str()), ("c2", "web.search", r#"{"q":"sei"}"#));
    assert_eq!(h.result("c2", "hits"), vec![Effect::CallLlm]);
    assert_eq!(h.contents().last().unwrap().1, "hits", "the result answers the call_tool call");
    // Arguments as a JSON string work too; unknown tools are an error.
    h.call(0, "c3", CALL_TOOL, r#"{"name":"web.scrape","arguments":"{\"url\":\"x\"}"}"#);
    h.call(1, "c4", CALL_TOOL, r#"{"name":"rm_rf","arguments":{}}"#);
    let fx = h.done();
    assert_eq!(tool_effect(&fx).function.arguments, r#"{"url":"x"}"#);
    assert!(h.contents().iter().any(|(_, c)| c.contains("there's no tool \"rm_rf\"")));
    h.crash();
}

#[test]
fn unloaded_tools_answer_with_their_schema() {
    let mut h = H::new(lazy_spec());
    h.user("find it");
    // Through call_tool, and by its own name: neither runs before it's loaded.
    h.call(0, "c1", CALL_TOOL, r#"{"name":"web.search","arguments":{"q":"x"}}"#);
    h.call(1, "c2", "world.say", r#"{}"#);
    let fx = h.done();
    assert_eq!(tool_effect(&fx).function.name, "world.say", "only the loaded tool runs");
    let msg = h.contents().iter().find(|(_, c)| c.contains("web.search wasn't loaded")).unwrap().1.clone();
    assert!(msg.contains(r#""name":"web.search""#), "{msg}");
    assert!(h.a.loaded.contains("web.search"));
    h.result("c2", "ok");
    // Once loaded, calling it by name works as well.
    h.call(0, "c3", "web__search", r#"{"q":"y"}"#);
    assert_eq!(tool_effect(&h.done()).function.name, "web.search");
    h.crash();
}

#[test]
fn approvals_see_the_real_tool() {
    let mut h = H::new(Spec { approve: vec!["web.scrape".into()], ..lazy_spec() });
    h.user("go");
    h.call(0, "c1", LOAD_TOOLS, r#"{"names":["web.scrape"]}"#);
    h.done();
    h.call(0, "c2", CALL_TOOL, r#"{"name":"web.scrape","arguments":{"url":"u"}}"#);
    let fx = h.done();
    assert!(matches!(&fx[..], [Effect::RequestApproval { call }] if call.function.name == "web.scrape"), "{fx:?}");
    assert_eq!(h.a.awaiting_approval()[0].function.name, "web.scrape");
    let fx = h.ev(Event::Approval { call_id: "c2".into(), approved: true });
    assert_eq!(tool_effect(&fx).function.name, "web.scrape");
}

#[test]
fn the_router_preloads_with_a_note() {
    let mut h = H::new(lazy_spec());
    assert!(h.ev(Event::ToolsLoaded { names: vec!["web.scrape".into(), "world.say".into(), "bogus".into()] }).is_empty());
    assert_eq!(h.a.loaded, BTreeSet::from(["web.scrape".to_string()]), "only lazy tools count as loaded");
    h.user("read this page");
    let msgs = h.contents();
    assert!(msgs[0].1.starts_with("[tools loaded for the next message") && msgs[0].1.contains(r#""name":"web.scrape""#), "{msgs:?}");
    assert_eq!(msgs[1].1, "read this page");
    // Loading it again adds no second note.
    h.ev(Event::ToolsLoaded { names: vec!["web.scrape".into()] });
    assert!(h.a.notes.is_empty());
    h.call(0, "c1", LOAD_TOOLS, "not json");
    h.done();
    assert!(h.contents().last().unwrap().1.starts_with("error: bad arguments"));
    h.crash();
    assert!(!names(&H::new(spec()).a.offered_tools()).contains(&LOAD_TOOLS), "nothing lazy: no load_tools");
}

#[test]
fn cached_prompt_tokens_count_at_their_weight() {
    let u = Usage { prompt_tokens: 1000, completion_tokens: 50, cached_prompt_tokens: 900 };
    assert_eq!(u.weighted(100), 1050);
    assert_eq!(u.weighted(10), 100 + 90 + 50);
    let mut h = H::new(Spec { budget: Budget { max_tokens: Some(1000), cached_percent: Some(10), ..Default::default() }, ..spec() });
    h.user("hi");
    h.ev(Event::LlmDelta { delta: Delta { content: Some("ok".into()), usage: Some(u), ..Default::default() } });
    h.done();
    assert_eq!(h.a.usage.cached_prompt_tokens, 900);
    assert_eq!(h.a.remaining_tokens(), Some(1000 - 240), "900 cached tokens count as 90");
}

#[test]
fn load_tools_and_its_use_in_one_message() {
    // The model loads a tool and calls it in the same message: load_tools
    // goes first, so the call runs.
    let mut h = H::new(lazy_spec());
    h.user("go");
    h.call(0, "c1", LOAD_TOOLS, r#"{"names":["web.search"]}"#);
    h.call(1, "c2", CALL_TOOL, r#"{"name":"web.search","arguments":{"q":"x"}}"#);
    let fx = h.done();
    assert_eq!(tool_effect(&fx).function.name, "web.search");
    h.result("c2", "hits");
    h.crash();
}

#[test]
fn two_calls_of_one_unloaded_tool() {
    // Both get the schema (judged against the same snapshot), neither runs.
    let mut h = H::new(lazy_spec());
    h.user("go");
    h.call(0, "c1", "web.search", r#"{"q":"a"}"#);
    h.call(1, "c2", CALL_TOOL, r#"{"name":"web.search","arguments":{"q":"b"}}"#);
    assert_eq!(h.done(), vec![Effect::CallLlm]);
    assert_eq!(h.contents().iter().filter(|(_, c)| c.contains("web.search wasn't loaded")).count(), 2);
    h.crash();
}

#[test]
fn a_panic_while_applying_fails_only_that_agent() {
    let mut h = H::new(spec());
    h.user("hi");
    h.text("partial");
    let fx = h.user("\u{0}panic");
    assert!(matches!(&h.a.phase, Phase::Failed { error } if error.contains("internal error applying a inbox event: test panic")), "{:?}", h.a.phase);
    assert!(matches!(&fx[..], [Effect::AbortInflight, Effect::Report { status: Status::Failed, .. }]), "{fx:?}");
    assert!(!in_guarded_apply(), "the guard is released");
    // Replaying the same log fails the same way, and the agent can be resumed.
    let replayed = Agent::replay(h.a.id, h.a.spec.clone(), &h.log);
    assert_eq!(replayed.phase, h.a.phase);
    assert_eq!(h.resume(), vec![Effect::CallLlm]);
}

fn used(h: &mut H, prompt: u64) {
    h.ev(Event::LlmDelta { delta: Delta { usage: Some(Usage { prompt_tokens: prompt, completion_tokens: 0, cached_prompt_tokens: 0 }), ..Default::default() } });
}

fn compacting_spec() -> Spec {
    let mut s = spec();
    s.tools = vec![ToolDef { name: "srv.t".into(), description: "A tool.".into(), parameters: json!({"type": "object"}) }];
    s.lazy = vec!["srv.t".into()];
    s.compact = Some(Compact { at_tokens: 1000, keep: 2, instructions: None, prompt: None });
    s
}

/// Runs until the context crosses the threshold: task, a load, two calls.
fn grown(h: &mut H) -> Vec<Effect> {
    assert_eq!(h.user("the task"), vec![Effect::CallLlm]);
    h.call(0, "c1", "load_tools", r#"{"names":["srv"]}"#);
    used(h, 400);
    assert_eq!(h.done(), vec![Effect::CallLlm], "load_tools is resolved in place");
    h.call(0, "c2", "call_tool", r#"{"name":"srv.t","arguments":{}}"#);
    used(h, 600);
    tool_effect(&h.done());
    assert_eq!(h.result("c2", "r2"), vec![Effect::CallLlm], "600 < 1000: no compaction yet");
    h.text("more");
    h.call(0, "c3", "call_tool", r#"{"name":"srv.t","arguments":{}}"#);
    used(h, 1200);
    tool_effect(&h.done());
    h.result("c3", "r3")
}

#[test]
fn compaction_summarises_all_but_the_task_and_the_last_messages() {
    let mut h = H::new(compacting_spec());
    // task, c1 call + result, c2 call + result, c3 call + result: keep 2 from c3's call.
    assert_eq!(grown(&mut h), vec![Effect::Compact { upto: 5 }]);
    let req = h.a.compaction_request("be useful", 5);
    let text = req[1].content.clone().unwrap();
    assert!(text.contains("be useful") && text.contains("the task") && text.contains("r2") && !text.contains("r3"), "{text}");
    // A crash while summarising: it's asked for again.
    assert_eq!(h.crash(), vec![Effect::Compact { upto: 5 }]);
    let before = h.a.usage.prompt_tokens;
    let fx = h.ev(Event::Compacted { upto: 5, summary: "the notes".into(), usage: Some(Usage { prompt_tokens: 100, completion_tokens: 50, cached_prompt_tokens: 0 }) });
    assert_eq!(fx, vec![Effect::CallLlm]);
    assert_eq!(h.a.usage.prompt_tokens, before + 100, "the summary is paid for");
    let c = h.contents();
    assert_eq!(c.len(), 5, "{c:?}");
    assert_eq!(c[0], (Role::User, "the task".into()));
    assert!(c[1].1.contains("the notes"));
    assert!(c[2].1.contains("srv.t"), "loaded schemas come back: {c:?}");
    assert_eq!((c[3].0, c[4].0), (Role::Assistant, Role::Tool), "the kept call stays with its result");
    assert_eq!(h.a.compactions, 1);
    // A stale summary (after a cancel, say) changes nothing.
    h.text("done");
    used(&mut h, 300);
    h.done();
    let n = h.a.messages.len();
    assert!(h.ev(Event::Compacted { upto: 3, summary: "late".into(), usage: None }).is_empty());
    assert_eq!(h.a.messages.len(), n);
    h.crash();
}

#[test]
fn a_failed_compaction_goes_on_without_one() {
    let mut h = H::new(compacting_spec());
    assert_eq!(grown(&mut h), vec![Effect::Compact { upto: 5 }]);
    assert_eq!(h.ev(Event::CompactFailed { error: "down".into() }), vec![Effect::CallLlm], "no second try this step");
    assert_eq!(h.a.messages.len(), 7);
    h.crash();
}

#[test]
fn a_type_says_what_its_summaries_keep() {
    let mut h = H::new(compacting_spec());
    grown(&mut h);
    assert_eq!(h.a.compaction_request("be useful", 5)[0].content.as_deref(), Some(COMPACT_PROMPT), "the fixed instructions alone");
    let mut s = compacting_spec();
    s.compact.as_mut().unwrap().instructions = Some("Keep how she feels about each person.".into());
    let mut h = H::new(s);
    grown(&mut h);
    let system = h.a.compaction_request("be useful", 5)[0].content.clone().unwrap();
    assert!(system.starts_with(COMPACT_PROMPT) && system.ends_with("For this agent in particular:\nKeep how she feels about each person."), "{system}");
    // Its own prompt instead of the built-in one, with the contract kept.
    let mut s = compacting_spec();
    s.compact.as_mut().unwrap().prompt = Some("Write her diary of these days.".into());
    let mut h = H::new(s);
    grown(&mut h);
    let system = h.a.compaction_request("be useful", 5)[0].content.clone().unwrap();
    assert_eq!(system, format!("Write her diary of these days.\n\n{COMPACT_CONTRACT}"));
    assert!(!system.contains("AI agent's conversation"));
}

#[test]
fn no_compaction_without_the_setting() {
    let mut s = compacting_spec();
    s.compact = None;
    let mut h = H::new(s);
    assert_eq!(grown(&mut h), vec![Effect::CallLlm]);
}

#[test]
fn the_full_history_keeps_what_compaction_dropped() {
    let mut h = H::new(compacting_spec());
    let before = |h: &H| h.a.messages.clone();
    assert_eq!(grown(&mut h), vec![Effect::Compact { upto: 5 }]);
    let all = before(&h);
    h.ev(Event::Compacted { upto: 5, summary: "first notes".into(), usage: None });
    // Grow again and compact a second time.
    h.text("more");
    h.call(0, "c4", "call_tool", r#"{"name":"srv.t","arguments":{}}"#);
    used(&mut h, 1500);
    tool_effect(&h.done());
    let fx = h.result("c4", "r4");
    let upto = match fx.as_slice() {
        [Effect::Compact { upto }] => *upto,
        other => panic!("{other:?}"),
    };
    h.ev(Event::Compacted { upto, summary: "second notes".into(), usage: None });
    let full = Agent::full_history(h.a.id, h.a.spec.clone(), &h.log);
    // The task, everything as it happened, nothing of the summaries' notes.
    assert_eq!(full.messages[0].content.as_deref(), Some("the task"));
    assert_eq!(&full.messages[..5], &all[..5], "the first compacted part, as it was");
    assert!(!full.messages.iter().any(|m| m.content.as_deref().is_some_and(|c| c.contains("[The conversation so far was compacted") || c.contains("[tools you loaded earlier"))));
    assert!(full.messages.iter().any(|m| m.content.as_deref() == Some("r4")), "and what came later");
    assert_eq!(full.compactions.len(), 2);
    assert_eq!((full.compactions[0].from, full.compactions[0].to, full.compactions[0].summary.as_str()), (1, 5, "first notes"));
    assert!(full.compactions[1].from == 5 && full.compactions[1].to > 5 && full.compactions[1].summary == "second notes");
    // What's left after the last compaction is the agent's tail.
    let tail = &h.a.messages[h.a.messages.len() - 2..];
    assert_eq!(&full.messages[full.messages.len() - 2..], tail);
    // Without compactions it's just the transcript.
    let mut h = H::new(spec());
    h.user("hello");
    h.text("hi");
    assert_eq!(Agent::full_history(h.a.id, h.a.spec.clone(), &h.log).messages, h.a.messages);
}

#[test]
fn grouped_events_come_as_one_message_by_route() {
    let mut h = H::new(Spec { group_events: true, ..spec() });
    let route = |r: &str| Addr::Route(r.into());
    // Busy with a task while events queue up.
    assert_eq!(h.user("the task"), vec![Effect::CallLlm]);
    for (r, e) in [("chat", r#"{"from": "alice",
        "text": "hi"}"#), ("world", r#"{"event": "joined"}"#), ("chat", r#"{"from": "bob", "text": "yo"}"#), ("world", "not json\nat all")] {
        h.ev(Event::Inbox { from: route(r), content: e.into(), reply: false });
    }
    h.ev(Event::Inbox { from: Addr::User("ann".into()), content: "a word".into(), reply: false });
    h.text("ok");
    h.done();
    let c = h.contents();
    let grouped = c.iter().find(|(_, t)| t.starts_with("[events from")).expect("one grouped message").1.clone();
    assert_eq!(grouped, "[events from route:chat]\n{\"from\":\"alice\",\"text\":\"hi\"}\n{\"from\":\"bob\",\"text\":\"yo\"}\n\n[events from route:world]\n{\"event\":\"joined\"}\nnot json\\nat all");
    assert!(c.iter().any(|(_, t)| t == "a word"), "messages from people stay their own");
    assert_eq!(c.iter().filter(|(_, t)| t.contains("[message from route")).count(), 0);
    // Without the flag, a message each, as before.
    let mut h = H::new(spec());
    h.user("the task");
    h.ev(Event::Inbox { from: route("chat"), content: "{}".into(), reply: false });
    h.ev(Event::Inbox { from: route("chat"), content: "{}".into(), reply: false });
    h.text("ok");
    h.done();
    assert_eq!(h.contents().iter().filter(|(_, t)| t.starts_with("[message from route:chat]")).count(), 2);
}
