//! Several hubs on one database: election, takeover, fencing.

mod common;

use std::time::Duration;

use common::db_url;
use serde_json::Value;
use subnet::hub::{Hub, http};
use subnet_core::addr::Addr;
use subnet_core::proto::Op;

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn one_leader_and_takeover_with_state() {
    let db = db_url().await;
    let a = Hub::start(&db, None, "http://a").await.unwrap();
    a.wait_leader().await;
    let b = Hub::start(&db, None, "http://b").await.unwrap();
    until("b sees a lead", || b.leader_url().as_deref() == Some("http://a")).await;
    assert!(!b.is_leader());
    a.op(&Addr::root(), Op::Send { to: Addr::Mailbox("m".into()), content: "kept".into() }).await.unwrap();

    a.shutdown();
    until("b takes over", || b.is_leader()).await;
    assert!(!a.is_leader());
    assert_eq!(b.leader_url().as_deref(), Some("http://b"));
    let mail = b.peek_mail(&Addr::Mailbox("m".into()), 10).await.unwrap();
    assert_eq!(mail[0].content, "kept");
}

#[tokio::test]
async fn deposed_leader_is_fenced_off() {
    let db = db_url().await;
    let a = Hub::start(&db, None, "http://a").await.unwrap();
    a.wait_leader().await;
    let text = "node \"s\" {}\nagent \"w\" {\n  credential {\n    base_url = \"http://x\"\n  }\n  model = \"m\"\n  nodes = [\"s\"]\n}\n";
    a.apply_cluster(vec![subnet::hub::db::ClusterFile { name: "c.hcl".into(), text: text.into() }], false, &Addr::root())
        .await
        .unwrap();
    common::net::node_ready(&a, "s").await;
    let id = common::id_of(&a.op(&Addr::root(), Op::Spawn { ty: "w".into(), prompt: "x".into(), tenant: None }).await.unwrap());
    // Another hub became leader behind a's back (e.g. a partition): the term moved on.
    let pool = sqlx::PgPool::connect(&db).await.unwrap();
    sqlx::query("update hub_leader set term = term + 1").execute(&pool).await.unwrap();
    let e = a.op(&Addr::root(), Op::Send { to: Addr::Agent(id), content: "late".into() }).await.unwrap_err();
    assert!(e.contains("not the leader"), "{e}");
    assert!(!a.is_leader(), "a stepped down");
    let n: i64 = sqlx::query_scalar("select count(*) from events where agent_id = $1").bind(id).fetch_one(&pool).await.unwrap();
    assert!(n <= 2, "no event written after the fence ({n})");
}

/// A hub listening on its own advertised URL.
async fn hub_at(db: &str) -> (std::sync::Arc<Hub>, String) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let hub = Hub::start(db, None, &url).await.unwrap();
    let h = hub.clone();
    tokio::spawn(async move { axum::serve(l, http::router(h)).await.unwrap() });
    (hub, url)
}

#[tokio::test]
async fn standby_answers_503_and_clients_follow_the_leader() {
    let db = db_url().await;
    let (a, a_url) = hub_at(&db).await;
    a.wait_leader().await;
    let (b, b_url) = hub_at(&db).await;
    until("b is a standby", || b.leader_url().is_some()).await;
    let r = reqwest::get(format!("{b_url}/v1/agents")).await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()[subnet_ops::client::LEADER_HEADER], a_url.as_str());
    // A client that only knows the standby follows the hint.
    let c = subnet::client::Client::new(&b_url, None);
    assert_eq!(c.call_raw("whoami", Value::Null).await.unwrap()["addr"], "user:root");
    // After a takeover the same client list keeps working.
    let both = subnet::client::Client::new(&format!("{a_url},{b_url}"), None);
    a.shutdown();
    until("b leads", || b.is_leader()).await;
    assert!(both.call_raw("list_agents", Value::Null).await.is_ok());
}
