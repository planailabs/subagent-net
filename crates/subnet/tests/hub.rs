mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{db_url, id_of, quiet, recv};
use serde_json::{Value, json};
use subnet::hub::{ConnId, Hub};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Budget, Event, PauseMode, Status};
use subnet_core::chat::Delta;
use subnet_core::proto::{Op, ToHub, ToSpawner, TypeInfo};
use tokio::sync::mpsc::UnboundedReceiver;

fn ty(name: &str, hash: &str) -> TypeInfo {
    TypeInfo {
        name: name.into(),
        hash: hash.into(),
        description: format!("{name} agent"),
        spawns: vec![],
        budget: Budget { max_tokens: None, max_depth: 0, max_children: 0 },
        approve: vec![],
    }
}

fn boss() -> TypeInfo {
    TypeInfo {
        spawns: vec!["worker".into()],
        budget: Budget { max_tokens: Some(1000), max_depth: 2, max_children: 2 },
        ..ty("boss", "b1")
    }
}

fn worker() -> TypeInfo {
    TypeInfo { budget: Budget { max_tokens: Some(400), max_depth: 5, max_children: 0 }, ..ty("worker", "w1") }
}

struct Sp {
    conn: ConnId,
    rx: UnboundedReceiver<ToSpawner>,
}

async fn spawner(hub: &Hub, name: &str, types: Vec<TypeInfo>, capacity: u32) -> Sp {
    let (conn, mut rx) = hub.connect(ToHub::Hello { name: name.into(), token: None, types, capacity }).await.unwrap();
    assert_eq!(recv(&mut rx).await, ToSpawner::Welcome);
    Sp { conn, rx }
}

async fn hub() -> Arc<Hub> {
    Hub::open(&db_url().await, None).await.unwrap()
}

async fn spawn(hub: &Hub, caller: &Addr, ty: &str) -> Result<AgentId, String> {
    hub.op(caller, Op::Spawn { ty: ty.into(), prompt: "go".into() }).await.map(|v| id_of(&v))
}

async fn expect_assign(sp: &mut Sp) -> (AgentId, u64, Vec<Event>) {
    match recv(&mut sp.rx).await {
        ToSpawner::Assign { agent, epoch, events, .. } => (agent, epoch, events),
        m => panic!("expected assign, got {m:?}"),
    }
}

async fn expect_commit(sp: &mut Sp) -> (AgentId, u64, Event) {
    match recv(&mut sp.rx).await {
        ToSpawner::Commit { agent, seq, event } => (agent, seq, event),
        m => panic!("expected commit, got {m:?}"),
    }
}

fn text(s: &str) -> Event {
    Event::LlmDelta { delta: Delta { content: Some(s.into()), ..Default::default() } }
}

async fn transcript(hub: &Hub, id: AgentId) -> Value {
    hub.op(&Addr::User, Op::Transcript { id }).await.unwrap()
}

#[tokio::test]
async fn spawned_agent_waits_for_a_spawner_then_is_assigned() {
    let hub = hub().await;
    assert!(spawn(&hub, &Addr::User, "worker").await.unwrap_err().contains("no live spawner"));
    let mut sp = spawner(&hub, "s1", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let (agent, epoch, events) = expect_assign(&mut sp).await;
    assert_eq!((agent, epoch), (id, 1));
    assert_eq!(events, vec![Event::Inbox { from: Addr::User, content: "go".into(), reply: false }, Event::Recovered]);
}

#[tokio::test]
async fn pending_agents_are_placed_when_spawner_connects() {
    let hub = hub().await;
    let mut a = spawner(&hub, "a", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    expect_assign(&mut a).await;
    hub.disconnect(a.conn).await;
    let mut b = spawner(&hub, "b", vec![worker()], 4).await;
    let (agent, epoch, events) = expect_assign(&mut b).await;
    // inbox, recovered (on a), recovered (on b)
    assert_eq!((agent, epoch, events.len()), (id, 2, 3));
}

#[tokio::test]
async fn proposals_are_committed_and_echoed() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let (_, epoch, _) = expect_assign(&mut sp).await;
    hub.handle(sp.conn, ToHub::Propose { agent: id, epoch, events: vec![text("hi"), Event::LlmDone] }).await.unwrap();
    assert_eq!(expect_commit(&mut sp).await, (id, 3, text("hi")));
    assert_eq!(expect_commit(&mut sp).await, (id, 4, Event::LlmDone));
    let t = transcript(&hub, id).await;
    assert_eq!(t["messages"][1]["content"], "hi");
    assert_eq!(t["phase"], "idle");
}

#[tokio::test]
async fn stale_epoch_writes_are_fenced() {
    let hub = hub().await;
    let mut a = spawner(&hub, "a", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    expect_assign(&mut a).await;
    let mut b = spawner(&hub, "b", vec![worker()], 4).await;
    // Simulate a partition: the hub gives the agent to b while a is still alive.
    hub.disconnect(a.conn).await;
    let (_, epoch, _) = expect_assign(&mut b).await;
    assert_eq!(epoch, 2);
    // a's late write, carrying epoch 1, is ignored.
    hub.handle(a.conn, ToHub::Propose { agent: id, epoch: 1, events: vec![text("stale")] }).await.unwrap();
    hub.handle(b.conn, ToHub::Propose { agent: id, epoch: 1, events: vec![text("stale")] }).await.unwrap();
    assert_eq!(recv(&mut b.rx).await, ToSpawner::Revoke { agent: id });
    assert_eq!(transcript(&hub, id).await["seq"], 3, "inbox + two recoveries, nothing stale");
}

#[tokio::test]
async fn spawner_may_only_propose_its_own_events() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let (_, epoch, _) = expect_assign(&mut sp).await;
    let forged = Event::Inbox { from: Addr::User, content: "forged".into(), reply: false };
    assert!(hub.handle(sp.conn, ToHub::Propose { agent: id, epoch, events: vec![forged] }).await.is_err());
    assert_eq!(transcript(&hub, id).await["seq"], 2);
}

#[tokio::test]
async fn placement_respects_type_hash_and_capacity() {
    let hub = hub().await;
    let mut old = spawner(&hub, "old", vec![ty("worker", "OLD")], 4).await;
    let mut sp = spawner(&hub, "new", vec![worker()], 1).await;
    let a = spawn(&hub, &Addr::User, "worker@w1").await.unwrap();
    assert_eq!(expect_assign(&mut sp).await.0, a);
    let b = spawn(&hub, &Addr::User, "worker@w1").await.unwrap();
    quiet(&mut sp.rx).await;
    quiet(&mut old.rx).await;
    let v = hub.op(&Addr::User, Op::ListAgents).await.unwrap();
    let pending = v.as_array().unwrap().iter().find(|x| id_of(x) == b).unwrap();
    assert_eq!(pending["spawner"], Value::Null);
}

#[tokio::test]
async fn least_loaded_spawner_wins() {
    let hub = hub().await;
    let mut a = spawner(&hub, "a", vec![worker()], 4).await;
    let mut b = spawner(&hub, "b", vec![worker()], 4).await;
    spawn(&hub, &Addr::User, "worker").await.unwrap();
    spawn(&hub, &Addr::User, "worker").await.unwrap();
    expect_assign(&mut a).await;
    expect_assign(&mut b).await;
}

#[tokio::test]
async fn bad_token_is_rejected() {
    let hub = Hub::open(&db_url().await, Some("secret".into())).await.unwrap();
    let hello =
        |t: Option<&str>| ToHub::Hello { name: "s".into(), token: t.map(Into::into), types: vec![], capacity: 1 };
    assert!(hub.connect(hello(None)).await.is_err());
    assert!(hub.connect(hello(Some("wrong"))).await.is_err());
    assert!(hub.connect(hello(Some("secret"))).await.is_ok());
}

#[tokio::test]
async fn send_to_agent_commits_inbox() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    expect_assign(&mut sp).await;
    let c = Addr::Client("x".into());
    hub.op(&c, Op::Send { to: Addr::Agent(id), content: "yo".into() }).await.unwrap();
    assert_eq!(expect_commit(&mut sp).await.2, Event::Inbox { from: c, content: "yo".into(), reply: false });
    let missing = Addr::Agent(uuid::Uuid::new_v4());
    assert!(hub.op(&Addr::User, Op::Send { to: missing, content: "?".into() }).await.is_err());
}

#[tokio::test]
async fn reports_go_to_parent_log_and_user_mail() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![boss(), worker()], 8).await;
    let boss_id = spawn(&hub, &Addr::User, "boss").await.unwrap();
    expect_assign(&mut sp).await;
    let kid = spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap();
    // Parent gets ChildSpawned, then the child is assigned.
    let (a, _, ev) = expect_commit(&mut sp).await;
    assert_eq!((a, ev), (boss_id, Event::ChildSpawned { id: kid, reserved: 400 }));
    let (_, kid_epoch, _) = expect_assign(&mut sp).await;

    // The user also asks the kid something mid-turn.
    hub.op(&Addr::User, Op::Send { to: Addr::Agent(kid), content: "and you?".into() }).await.unwrap();
    expect_commit(&mut sp).await;
    let answer = |s: &str| ToHub::Propose { agent: kid, epoch: kid_epoch, events: vec![text(s), Event::LlmDone] };
    hub.handle(sp.conn, answer("done")).await.unwrap();
    let mut got_report = false;
    // kid: delta, done; boss: child report
    for _ in 0..3 {
        let (a, _, ev) = expect_commit(&mut sp).await;
        if a == boss_id {
            assert_eq!(ev, Event::ChildReport { id: kid, status: Status::Idle, content: "done".into() });
            got_report = true;
        }
    }
    assert!(got_report);
    assert_eq!(hub.op(&Addr::User, Op::WaitInbox { timeout_ms: Some(50) }).await.unwrap(), json!([]));
    // Second turn answers the user (and the parent again).
    hub.handle(sp.conn, answer("me too")).await.unwrap();
    let mail = hub.op(&Addr::User, Op::WaitInbox { timeout_ms: Some(1000) }).await.unwrap();
    assert_eq!(mail, json!([{"from": format!("agent:{kid}"), "content": "me too", "status": "idle"}]));
}

#[tokio::test]
async fn cancel_reports_to_parent_even_though_spawner_is_revoked() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![boss(), worker()], 8).await;
    let boss_id = spawn(&hub, &Addr::User, "boss").await.unwrap();
    let kid = spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap();
    hub.op(&Addr::User, Op::Cancel { id: kid }).await.unwrap();
    let t = transcript(&hub, boss_id).await;
    assert_eq!(t["phase"], "thinking", "the report woke the boss");
    let _ = &mut sp;
}

#[tokio::test]
async fn unlimited_child_type_gets_half_of_parent_budget() {
    let hub = hub().await;
    let mut w = worker();
    w.budget.max_tokens = None;
    let _sp = spawner(&hub, "s", vec![boss(), w], 8).await;
    let boss_id = spawn(&hub, &Addr::User, "boss").await.unwrap();
    let kid = spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap();
    let rows = hub.op(&Addr::User, Op::ListAgents).await.unwrap();
    assert!(rows.as_array().unwrap().iter().any(|r| id_of(r) == kid));
    assert_eq!(transcript(&hub, boss_id).await["reserved"], 500);
}

#[tokio::test]
async fn wait_inbox_wakes_on_new_mail() {
    let hub = hub().await;
    let c = Addr::Client("sess".into());
    let h2 = hub.clone();
    let c2 = c.clone();
    let waiter = tokio::spawn(async move { h2.op(&c2, Op::WaitInbox { timeout_ms: Some(5000) }).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    hub.op(&Addr::User, Op::Send { to: c.clone(), content: "psst".into() }).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap();
    assert_eq!(got[0]["content"], "psst");
    assert_eq!(got[0]["from"], "user");
    // Mail is taken exactly once.
    assert_eq!(hub.op(&c, Op::WaitInbox { timeout_ms: Some(10) }).await.unwrap(), json!([]));
}

#[tokio::test]
async fn agents_cannot_wait_inbox() {
    let hub = hub().await;
    assert!(hub.op(&Addr::Agent(uuid::Uuid::new_v4()), Op::WaitInbox { timeout_ms: None }).await.is_err());
}

#[tokio::test]
async fn spawn_permissions_and_budgets() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![boss(), worker()], 16).await;
    let w = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let b = spawn(&hub, &Addr::User, "boss").await.unwrap();
    // worker's type may not spawn anything
    assert!(spawn(&hub, &Addr::Agent(w), "worker").await.unwrap_err().contains("may not spawn"));
    // boss may not spawn boss
    assert!(spawn(&hub, &Addr::Agent(b), "boss").await.unwrap_err().contains("may not spawn"));
    let k1 = spawn(&hub, &Addr::Agent(b), "worker").await.unwrap();
    spawn(&hub, &Addr::Agent(b), "worker").await.unwrap();
    assert!(spawn(&hub, &Addr::Agent(b), "worker").await.unwrap_err().contains("child budget"));
    // The child is placed like any other agent.
    let mut assigned = vec![];
    while assigned.len() < 4 {
        if let ToSpawner::Assign { agent, .. } = recv(&mut sp.rx).await {
            assigned.push(agent);
        }
    }
    assert!(assigned.contains(&k1));
}

#[tokio::test]
async fn token_reservation_limits_children() {
    let hub = hub().await;
    let mut b = boss();
    b.budget.max_tokens = Some(500);
    b.budget.max_children = 5;
    let _sp = spawner(&hub, "s", vec![b, worker()], 16).await;
    let boss_id = spawn(&hub, &Addr::User, "boss").await.unwrap();
    spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap(); // reserves 400
    spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap(); // reserves the last 100
    assert!(spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap_err().contains("token budget"));
}

#[tokio::test]
async fn depth_budget_stops_grandchildren() {
    let hub = hub().await;
    let mut b = boss();
    b.spawns = vec!["boss".into()];
    b.budget.max_depth = 1;
    let _sp = spawner(&hub, "s", vec![b], 16).await;
    let root = spawn(&hub, &Addr::User, "boss").await.unwrap();
    let child = spawn(&hub, &Addr::Agent(root), "boss").await.unwrap();
    assert!(spawn(&hub, &Addr::Agent(child), "boss").await.unwrap_err().contains("depth"));
}

#[tokio::test]
async fn pause_tree_reaches_descendants_and_agents_only_control_their_own() {
    let hub = hub().await;
    let _sp = spawner(&hub, "s", vec![boss(), worker()], 16).await;
    let root = spawn(&hub, &Addr::User, "boss").await.unwrap();
    let kid = spawn(&hub, &Addr::Agent(root), "worker").await.unwrap();
    let other = spawn(&hub, &Addr::User, "worker").await.unwrap();
    hub.op(&Addr::User, Op::Pause { id: root, mode: PauseMode::Quick, tree: true }).await.unwrap();
    for id in [root, kid] {
        assert_eq!(transcript(&hub, id).await["pause"], "quick");
    }
    assert_eq!(transcript(&hub, other).await["pause"], Value::Null);
    let e = hub.op(&Addr::Agent(kid), Op::Pause { id: other, mode: PauseMode::Hard, tree: false }).await.unwrap_err();
    assert!(e.contains("not a descendant"));
    assert!(hub.op(&Addr::Agent(kid), Op::Resume { id: root, tree: false }).await.is_err());
    hub.op(&Addr::Agent(root), Op::Resume { id: kid, tree: false }).await.unwrap();
    assert_eq!(transcript(&hub, kid).await["pause"], Value::Null);
}

#[tokio::test]
async fn cancel_revokes_subtree_and_frees_capacity() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![boss(), worker()], 2).await;
    let root = spawn(&hub, &Addr::User, "boss").await.unwrap();
    expect_assign(&mut sp).await;
    let kid = spawn(&hub, &Addr::Agent(root), "worker").await.unwrap();
    expect_commit(&mut sp).await; // ChildSpawned
    expect_assign(&mut sp).await;
    hub.op(&Addr::User, Op::Cancel { id: root }).await.unwrap();
    let mut revoked = vec![];
    while revoked.len() < 2 {
        if let ToSpawner::Revoke { agent } = recv(&mut sp.rx).await {
            revoked.push(agent);
        }
    }
    revoked.sort();
    let mut want = vec![root, kid];
    want.sort();
    assert_eq!(revoked, want);
    // Capacity is free again.
    let n = spawn(&hub, &Addr::User, "worker").await.unwrap();
    loop {
        if let ToSpawner::Assign { agent, .. } = recv(&mut sp.rx).await {
            assert_eq!(agent, n);
            break;
        }
    }
}

#[tokio::test]
async fn approve_commits_approval() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 2).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    expect_assign(&mut sp).await;
    hub.op(&Addr::User, Op::Approve { id, call_id: "c".into(), approved: true }).await.unwrap();
    assert_eq!(expect_commit(&mut sp).await.2, Event::Approval { call_id: "c".into(), approved: true });
}

#[tokio::test]
async fn hub_restart_reloads_state_and_keeps_fencing() {
    let url = db_url().await;
    let hub = Hub::open(&url, None).await.unwrap();
    let mut sp = spawner(&hub, "s", vec![worker()], 2).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let (_, epoch, _) = expect_assign(&mut sp).await;
    hub.handle(sp.conn, ToHub::Propose { agent: id, epoch, events: vec![text("partial")] }).await.unwrap();
    let before = transcript(&hub, id).await;
    drop(hub);

    let hub = Hub::open(&url, None).await.unwrap();
    let after = transcript(&hub, id).await;
    assert_eq!(after["partial"], before["partial"]);
    assert_eq!(after["seq"], 3);
    let mut sp = spawner(&hub, "s", vec![worker()], 2).await;
    let (_, epoch2, events) = expect_assign(&mut sp).await;
    assert_eq!(epoch2, epoch + 1, "epoch survives restarts");
    assert_eq!(events.len(), 4);
    assert_eq!(events[3], Event::Recovered);
}

#[tokio::test]
async fn fork_copies_a_prefix() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 4).await;
    let id = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let (_, epoch, _) = expect_assign(&mut sp).await;
    hub.handle(sp.conn, ToHub::Propose { agent: id, epoch, events: vec![text("a"), Event::LlmDone] }).await.unwrap();
    // inbox, recovered, "a" | done
    let f = id_of(&hub.op(&Addr::User, Op::Fork { id, at: Some(3) }).await.unwrap());
    let t = transcript(&hub, f).await;
    assert_eq!(t["seq"], 4, "the fork is placed, which logs a recovery");
    assert_eq!(t["partial"]["content"], "a");
    assert_eq!(t["parent"], Value::Null);
    assert!(hub.op(&Addr::Agent(id), Op::Fork { id, at: None }).await.is_err());
}

#[tokio::test]
async fn list_types_aggregates_spawners() {
    let hub = hub().await;
    let _a = spawner(&hub, "a", vec![worker(), boss()], 3).await;
    let _b = spawner(&hub, "b", vec![worker()], 2).await;
    let v = hub.op(&Addr::User, Op::ListTypes).await.unwrap();
    let w = v.as_array().unwrap().iter().find(|t| t["name"] == "worker").unwrap();
    assert_eq!((w["spawners"].as_u64(), w["free"].as_u64()), (Some(2), Some(5)));
}

async fn finish_turn(hub: &Hub, sp: &mut Sp, id: AgentId, epoch: u64, s: &str) {
    hub.handle(sp.conn, ToHub::Propose { agent: id, epoch, events: vec![text(s), Event::LlmDone] }).await.unwrap();
}

/// Drains messages until an Assign arrives; returns it and any Revokes seen.
async fn next_assign(sp: &mut Sp) -> ((AgentId, u64), Vec<AgentId>) {
    let mut revoked = vec![];
    loop {
        match recv(&mut sp.rx).await {
            ToSpawner::Assign { agent, epoch, .. } => return ((agent, epoch), revoked),
            ToSpawner::Revoke { agent } => revoked.push(agent),
            _ => {}
        }
    }
}

#[tokio::test]
async fn idle_agents_are_evicted_for_agents_with_work() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 1).await;
    let a = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let ((_, ea), _) = next_assign(&mut sp).await;
    finish_turn(&hub, &mut sp, a, ea, "a done").await;
    // b needs the only slot: a is idle, so it's evicted.
    let b = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let ((got, eb), revoked) = next_assign(&mut sp).await;
    assert_eq!((got, revoked), (b, vec![a]));
    // a gets a message while b is busy: it waits for a slot...
    hub.op(&Addr::User, Op::Send { to: Addr::Agent(a), content: "again".into() }).await.unwrap();
    quiet(&mut sp.rx).await;
    assert_eq!(transcript(&hub, a).await["spawner"], Value::Null);
    // ...which frees up as soon as b goes idle.
    finish_turn(&hub, &mut sp, b, eb, "b done").await;
    let ((got, ea2), revoked) = next_assign(&mut sp).await;
    assert_eq!((got, revoked), (a, vec![b]));
    assert_eq!(ea2, ea + 1);
}

#[tokio::test]
async fn paused_agents_give_up_their_slot_and_come_back_on_resume() {
    let hub = hub().await;
    let mut sp = spawner(&hub, "s", vec![worker()], 1).await;
    let a = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let ((_, ea), _) = next_assign(&mut sp).await;
    hub.op(&Addr::User, Op::Pause { id: a, mode: PauseMode::Hard, tree: false }).await.unwrap();
    hub.handle(sp.conn, ToHub::Propose { agent: a, epoch: ea, events: vec![text("par"), Event::LlmAborted] })
        .await
        .unwrap();
    assert_eq!(transcript(&hub, a).await["paused"], true);
    let b = spawn(&hub, &Addr::User, "worker").await.unwrap();
    let ((got, _), revoked) = next_assign(&mut sp).await;
    assert_eq!((got, revoked), (b, vec![a]));
    hub.op(&Addr::User, Op::Resume { id: a, tree: false }).await.unwrap();
    // b is busy (thinking): a waits; cancel b and a gets the slot.
    hub.op(&Addr::User, Op::Cancel { id: b }).await.unwrap();
    let ((got, _), _) = next_assign(&mut sp).await;
    assert_eq!(got, a);
    assert_eq!(transcript(&hub, a).await["partial"]["content"], "par");
}

#[tokio::test]
async fn agents_waiting_on_children_do_not_hold_a_slot() {
    let hub = hub().await;
    let mut b = boss();
    b.budget.max_tokens = None;
    let mut sp = spawner(&hub, "s", vec![b, worker()], 1).await;
    let boss_id = spawn(&hub, &Addr::User, "boss").await.unwrap();
    let ((_, eb), _) = next_assign(&mut sp).await;
    let kid = spawn(&hub, &Addr::Agent(boss_id), "worker").await.unwrap();
    // The boss is thinking, so the kid can't run yet.
    quiet_except_commits(&mut sp).await;
    let call = subnet_core::chat::ToolCallDelta {
        index: 0,
        id: Some("w".into()),
        name: Some("wait_for".into()),
        arguments: Some(json!({"ids": [kid]}).to_string()),
    };
    let wait = Event::LlmDelta { delta: Delta { tool_calls: vec![call], ..Default::default() } };
    hub.handle(sp.conn, ToHub::Propose { agent: boss_id, epoch: eb, events: vec![wait, Event::LlmDone] })
        .await
        .unwrap();
    let ((got, _), revoked) = next_assign(&mut sp).await;
    assert_eq!((got, revoked), (kid, vec![boss_id]));
}

async fn quiet_except_commits(sp: &mut Sp) {
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), sp.rx.recv()).await {
        assert!(matches!(m, ToSpawner::Commit { .. }), "unexpected {m:?}");
    }
}
