//! Nodes over the hub's WebSocket.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::db_url;
use common::llm::MockLlm;
use common::net::{agent, wait_configured};
use futures::{SinkExt, StreamExt};
use subnet::hub::db::ClusterFile;
use subnet::hub::{Hub, http};
use subnet::node::Node;
use subnet::wire::{AgentStatus, ToHub, ToNode};
use subnet_core::addr::Addr;
use subnet_core::proto::Op;
use tokio_tungstenite::tungstenite::Message;

async fn serve(hub: Arc<Hub>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, http::router(hub)).await.unwrap() });
    format!("http://{addr}")
}

async fn hub_with(cluster: &str, admin: Option<&str>) -> Arc<Hub> {
    let hub = Hub::open(&db_url().await, admin.map(Into::into)).await.unwrap();
    let files = vec![ClusterFile { name: "c.hcl".into(), text: cluster.into() }];
    hub.apply_cluster(files, false, &Addr::root()).await.unwrap();
    hub
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(ws: &mut Ws, m: &ToHub) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into())).await.unwrap();
}

async fn next(ws: &mut Ws) -> ToNode {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn raw_node_protocol_and_reassignment_on_disconnect() {
    let cluster = format!(
        "node \"remote\" {{}}\nnode \"local\" {{}}\n{}",
        agent("worker", "w", "http://unused", &["remote", "local"], "")
    );
    let hub = hub_with(&cluster, Some("tok")).await;
    let base = serve(hub.clone()).await;
    let url = subnet::node::ws::node_url(&base);

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut ws, &ToHub::Hello { name: "remote".into(), token: Some("tok".into()) }).await;
    assert_eq!(next(&mut ws).await, ToNode::Welcome);
    let ToNode::Configure { config } = next(&mut ws).await else { panic!("expected configure") };
    assert_eq!(config.agents[0].name, "worker");
    let ready = ToHub::Ready { agents: vec![AgentStatus { id: config.agents[0].id.clone(), error: None }], mcps: vec![] };
    send(&mut ws, &ready).await;
    wait_configured(&hub, "remote").await;

    let spawned = hub.op(&Addr::root(), Op::Spawn { ty: "worker".into(), prompt: "hi".into(), tenant: None }).await.unwrap();
    let id = common::id_of(&spawned);
    let ToNode::Assign { agent, epoch, .. } = next(&mut ws).await else { panic!() };
    assert_eq!(agent, id);

    // Requests over the socket get replies.
    send(&mut ws, &ToHub::Request { id: 9, agent: id, epoch, op: Op::ListTypes }).await;
    let ToNode::Reply { id: 9, result: Ok(types) } = next(&mut ws).await else { panic!() };
    assert_eq!(types[0]["name"], "worker");

    // Dropping the socket moves the agent to another node.
    drop(ws);
    common::net::node_ready(&hub, "local").await;
    for _ in 0..100 {
        let t = hub.op(&Addr::root(), Op::Transcript { id }).await.unwrap();
        if t["node"] == "local" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("agent never moved to the local node");
}

#[tokio::test]
async fn node_tokens_and_names_are_checked() {
    let hub = hub_with("node \"n\" {}\n", Some("tok")).await;
    let base = serve(hub.clone()).await;
    let url = subnet::node::ws::node_url(&base);
    let hello = |name: &str, token: Option<&str>| ToHub::Hello { name: name.into(), token: token.map(Into::into) };
    for (name, token) in [("n", None), ("n", Some("wrong")), ("ghost", Some("nope"))] {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        send(&mut ws, &hello(name, token)).await;
        assert!(matches!(next(&mut ws).await, ToNode::Rejected { .. }), "{name} {token:?}");
    }
    // A node token works only for its own name.
    let t = hub.issue_token(serde_json::from_str("\"node\"").unwrap(), "n").await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut ws, &hello("n", Some(&t))).await;
    assert_eq!(next(&mut ws).await, ToNode::Welcome);
    // The same name can't connect twice.
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send(&mut ws2, &hello("n", Some(&t))).await;
    assert!(matches!(next(&mut ws2).await, ToNode::Rejected { .. }));
    // Node tokens don't work for the API.
    assert!(hub.authenticate(Some(&t)).is_err());
}

#[tokio::test]
async fn real_node_over_websocket_answers() {
    let llm = MockLlm::start().await;
    llm.say("sys", &["over the wire"]);
    let hub = hub_with(&format!("node \"w\" {{}}\n{}", agent("worker", "sys", &llm.url, &["w"], "")), None).await;
    let base = serve(hub.clone()).await;
    let node = Arc::new(Node::new("w", None));
    // The first hub URL is dead: the node moves on to the next.
    let hubs = format!("http://127.0.0.1:1,{base}");
    let task = tokio::spawn(async move { subnet::node::ws::run(node, &hubs).await });
    wait_configured(&hub, "w").await;
    hub.op(&Addr::root(), Op::Spawn { ty: "worker".into(), prompt: "hi".into(), tenant: None }).await.unwrap();
    let mail = hub.op(&Addr::root(), Op::WaitInbox { timeout_ms: Some(5000) }).await.unwrap();
    assert_eq!(mail[0]["content"], "over the wire");
    task.abort();
}

#[tokio::test]
async fn rejected_node_stops() {
    let hub = hub_with("node \"n\" {}\n", Some("tok")).await;
    let base = serve(hub).await;
    let node = Arc::new(Node::new("n", Some("wrong".into())));
    let r = tokio::time::timeout(Duration::from_secs(5), subnet::node::ws::run(node, &base)).await.unwrap();
    assert!(r.unwrap_err().to_string().contains("bad token"));
}
