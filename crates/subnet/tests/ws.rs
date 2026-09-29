mod common;

use std::sync::Arc;

use common::{db_url, id_of, recv};
use futures::{SinkExt, StreamExt};
use subnet::hub::{Hub, http};
use subnet_core::addr::Addr;
use subnet_core::agent::Budget;
use subnet_core::proto::{Op, ToHub, ToSpawner, TypeInfo};
use tokio_tungstenite::tungstenite::Message;

fn worker() -> TypeInfo {
    TypeInfo {
        name: "worker".into(),
        hash: "w".into(),
        description: String::new(),
        spawns: vec![],
        budget: Budget::default(),
        approve: vec![],
    }
}

async fn serve(hub: Arc<Hub>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, http::router(hub)).await.unwrap() });
    format!("ws://{addr}/spawner")
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(ws: &mut Ws, m: &ToHub) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into())).await.unwrap();
}

async fn next(ws: &mut Ws) -> ToSpawner {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn spawner_over_websocket_gets_assignments_and_loses_them_on_disconnect() {
    let hub = Hub::open(&db_url().await, Some("tok".into())).await.unwrap();
    let url = serve(hub.clone()).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut ws, &ToHub::Hello { name: "remote".into(), token: Some("tok".into()), types: vec![worker()], capacity: 2 }).await;
    assert_eq!(next(&mut ws).await, ToSpawner::Welcome);

    let id = id_of(&hub.op(&Addr::User, Op::Spawn { ty: "worker".into(), prompt: "hi".into() }).await.unwrap());
    let ToSpawner::Assign { agent, epoch, .. } = next(&mut ws).await else { panic!() };
    assert_eq!(agent, id);

    // Requests over the socket get replies.
    send(&mut ws, &ToHub::Request { id: 9, agent: id, epoch, op: Op::ListTypes }).await;
    let ToSpawner::Reply { id: 9, result: Ok(types) } = next(&mut ws).await else { panic!() };
    assert_eq!(types[0]["name"], "worker");

    // Dropping the socket moves the agent to another spawner.
    drop(ws);
    let (_, mut rx) = hub
        .connect(ToHub::Hello { name: "local".into(), token: Some("tok".into()), types: vec![worker()], capacity: 2 })
        .await
        .unwrap();
    assert_eq!(recv(&mut rx).await, ToSpawner::Welcome);
    let ToSpawner::Assign { agent, epoch: e2, .. } = recv(&mut rx).await else { panic!() };
    assert_eq!((agent, e2), (id, epoch + 1));
}

#[tokio::test]
async fn bad_hello_is_rejected() {
    let hub = Hub::open(&db_url().await, Some("tok".into())).await.unwrap();
    let url = serve(hub).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut ws, &ToHub::Hello { name: "x".into(), token: Some("nope".into()), types: vec![], capacity: 1 }).await;
    assert!(matches!(next(&mut ws).await, ToSpawner::Rejected { .. }));

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::Text("{\"t\":\"garbage\"}".into())).await.unwrap();
    assert!(matches!(next(&mut ws).await, ToSpawner::Rejected { .. }));
}

fn spawner_cfg(llm_url: &str, token_env: Option<&str>) -> subnet::spawner::config::Config {
    let t = subnet::spawner::config::TypeConfig {
        name: "worker".into(),
        description: String::new(),
        system: "sys".into(),
        model: subnet_llm::ModelConfig {
            base_url: llm_url.into(),
            model: "m".into(),
            api_key_env: None,
            prefill: false,
            params: Default::default(),
        },
        mcp: vec![],
        spawns: vec![],
        budget: Budget::default(),
        approve: vec![],
        idempotent: vec![],
    };
    subnet::spawner::config::Config {
        hub: String::new(),
        name: "ws-spawner".into(),
        capacity: 2,
        token_env: token_env.map(Into::into),
        types: vec![t],
    }
}

#[tokio::test]
async fn real_spawner_over_websocket_answers() {
    let llm = common::llm::MockLlm::start().await;
    llm.say("sys", &["over the wire"]);
    let hub = Hub::open(&db_url().await, None).await.unwrap();
    let url = serve(hub.clone()).await;
    let sp = Arc::new(subnet::spawner::Spawner::new(&spawner_cfg(&llm.url, None)).unwrap());
    let task = tokio::spawn(async move { subnet::spawner::ws::run(sp, &url).await });
    // Wait for the type to show up.
    for _ in 0..100 {
        if !hub.op(&Addr::User, Op::ListTypes).await.unwrap().as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    hub.op(&Addr::User, Op::Spawn { ty: "worker".into(), prompt: "hi".into() }).await.unwrap();
    let mail = hub.op(&Addr::User, Op::WaitInbox { timeout_ms: Some(5000) }).await.unwrap();
    assert_eq!(mail[0]["content"], "over the wire");
    task.abort();
}

#[tokio::test]
async fn rejected_spawner_stops() {
    let hub = Hub::open(&db_url().await, Some("tok".into())).await.unwrap();
    let url = serve(hub).await;
    let sp = Arc::new(subnet::spawner::Spawner::new(&spawner_cfg("http://unused", None)).unwrap());
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), subnet::spawner::ws::run(sp, &url)).await.unwrap();
    assert!(r.unwrap_err().to_string().contains("bad token"));
}
