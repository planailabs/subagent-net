//! The switchboard end to end: events → routes → deliveries.

mod common;

use std::time::Duration;

use common::llm::text;
use common::net::{Net, agent};
use serde_json::{Value, json};
use subnet::hub::db::ClusterFile;
use subnet_core::addr::Addr;

const HELPER: &str = "You handle events.";
const CONCIERGE: &str = "You are the concierge.";

fn cluster(routes: &str) -> String {
    format!(
        "node \"s\" {{}}\n{}{}resident \"concierge\" {{\n  mixture = \"clerk\"\n}}\nsense \"door\" {{\n  node = \"s\"\n  source {{ webhook = {{ path = \"/door\" }} }}\n}}\n{routes}",
        agent("helper", HELPER, "{llm}", &["s"], ""),
        agent("clerk", CONCIERGE, "{llm}", &["s"], ""),
    )
}

async fn net(routes: &str) -> Net {
    let n = Net::new(&cluster(routes)).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    n.mail().await; // the resident's greeting
    n
}

async fn wait_deliveries(n: &Net, route: &str, count: usize) -> Vec<Value> {
    for _ in 0..200 {
        let d = n.hub.list_deliveries(Some(route), 100).await.unwrap();
        if d.len() >= count {
            return d.into_iter().rev().map(|r| json!({"payload": r.payload, "outcomes": r.outcomes})).collect();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("fewer than {count} deliveries for {route}");
}

async fn route(n: &Net, name: &str) -> Value {
    let all = serde_json::to_value(n.hub.list_routes().await).unwrap();
    all.as_array().unwrap().iter().find(|r| r["name"] == name).unwrap().clone()
}

#[tokio::test]
async fn mailbox_delivery_with_when_and_map() {
    let n = net("route \"opened\" {\n  from = \"door\"\n  when = \"event.state == 'open'\"\n  map = \"{'who': event.card}\"\n  deliver { mailbox = \"door-log\" }\n}\n").await;
    let mut notices = n.hub.subscribe();
    n.hub.inject_event("door", json!({"state":"closed"})).unwrap();
    n.hub.inject_event("door", json!({"state":"open","card":"x7"})).unwrap();
    let d = wait_deliveries(&n, "opened", 1).await;
    assert_eq!(d[0]["payload"]["event"], json!({"who":"x7"}));
    assert_eq!(d[0]["outcomes"][0]["ok"], "stored");
    let mail = n.hub.peek_mail(&Addr::Mailbox("door-log".into()), 10).await.unwrap();
    assert_eq!(mail[0].content, r#"{"who":"x7"}"#);
    assert_eq!(mail[0].from, Addr::Route("opened".into()));
    let r = route(&n, "opened").await;
    assert_eq!((r["counters"]["seen"].as_u64(), r["counters"]["filtered"].as_u64()), (Some(2), Some(1)));
    // Subscribers saw the events and the delivery.
    let mut kinds = vec![];
    while let Ok(x) = notices.try_recv() {
        kinds.push(serde_json::to_value(&x).unwrap()["kind"].as_str().unwrap().to_string());
    }
    assert!(kinds.contains(&"sense".to_string()) && kinds.contains(&"delivery".to_string()), "{kinds:?}");
}

#[tokio::test]
async fn send_to_a_resident() {
    let n = net("route \"tell\" {\n  from = \"door\"\n  deliver { send = \"concierge\" }\n}\n").await;
    n.llm.push(CONCIERGE, |body| {
        let last = body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
        text(&[&format!("noted: {last}")])
    });
    n.hub.inject_event("door", json!({"state":"open"})).unwrap();
    // The resident answers to the route; route answers are kept at route:<name>.
    for _ in 0..200 {
        let m = n.hub.peek_mail(&Addr::Route("tell".into()), 10).await.unwrap();
        if let Some(m) = m.first() {
            assert!(m.content.starts_with("noted: [message from route:tell]"), "{}", m.content);
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no answer from the resident");
}

#[tokio::test]
async fn spawn_per_event_with_max_active() {
    let n = net("route \"handle\" {\n  from = \"door\"\n  max_active = 1\n  deliver {\n    spawn = \"helper\"\n    prompt = \"'door is ' + event.state\"\n  }\n}\n").await;
    n.llm.set_gap(Duration::from_millis(100));
    n.llm.push(HELPER, |body| text(&[&format!("handled {}", body["messages"][1]["content"].as_str().unwrap())]));
    n.llm.push(HELPER, |body| text(&[&format!("handled {}", body["messages"][1]["content"].as_str().unwrap())]));
    n.hub.inject_event("door", json!({"state":"open"})).unwrap();
    n.hub.inject_event("door", json!({"state":"closed"})).unwrap();
    let d = wait_deliveries(&n, "handle", 2).await;
    let outs: Vec<_> = d.iter().map(|x| x["outcomes"][0]["ok"].clone()).collect();
    assert!(outs.contains(&json!("queued")), "the second waits for the first: {outs:?}");
    for _ in 0..400 {
        let m = n.hub.peek_mail(&Addr::Route("handle".into()), 10).await.unwrap();
        if m.len() == 2 {
            let mut c: Vec<_> = m.iter().map(|m| m.content.clone()).collect();
            c.sort();
            assert_eq!(c, ["handled [message from route:handle]\ndoor is closed", "handled [message from route:handle]\ndoor is open"]);
            assert_eq!(route(&n, "handle").await["active"], 0);
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("both events should have been handled in turn");
}

#[tokio::test]
async fn batch_window_delivers_once() {
    let n = net("route \"burst\" {\n  from = \"door\"\n  batch = { window = \"300ms\" }\n  deliver { mailbox = \"bursts\" }\n}\n").await;
    for i in 0..3 {
        n.hub.inject_event("door", json!({"i": i})).unwrap();
    }
    let d = wait_deliveries(&n, "burst", 1).await;
    assert_eq!(d[0]["payload"]["batch"], json!([{"i":0},{"i":1},{"i":2}]));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(n.hub.list_deliveries(Some("burst"), 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_delivery_calls_a_tool() {
    let cluster = format!(
        "{}mcp \"echo\" {{\n  command = [{:?}]\n  nodes = [\"s\"]\n}}\nroute \"store\" {{\n  from = \"door\"\n  deliver {{\n    mcp = {{ server = \"echo\", tool = \"echo\", args = \"{{'text': event.state}}\" }}\n  }}\n}}\n",
        cluster(""),
        common::example("mcp_echo")
    );
    let n = Net::new(&cluster).await;
    n.llm.say(CONCIERGE, &["on duty"]);
    n.node("s").await;
    n.hub.inject_event("door", json!({"state":"ajar"})).unwrap();
    let d = wait_deliveries(&n, "store", 1).await;
    assert_eq!(d[0]["outcomes"][0]["ok"], "stdio echo: ajar", "{d:?}");
}

#[tokio::test]
async fn real_sense_feeds_a_route() {
    let kit = common::example("sensekit");
    let n = Net::new(&format!(
        "node \"s\" {{}}\nsense \"nums\" {{\n  node = \"s\"\n  source {{ exec = [{kit:?}, \"emit\", \"{{\\\"n\\\":1}}\", \"{{\\\"n\\\":5}}\"] }}\n}}\nroute \"big\" {{\n  from = \"nums\"\n  when = \"event.n > 2\"\n  deliver {{ mailbox = \"big\" }}\n}}\n"
    ))
    .await;
    n.node("s").await;
    let d = wait_deliveries(&n, "big", 1).await;
    assert_eq!(d[0]["payload"]["event"], json!({"n":5}));
    let senses = serde_json::to_value(n.hub.list_senses().await).unwrap();
    assert_eq!(senses[0]["running"], true, "{senses}");
}

#[tokio::test]
async fn invalid_cel_rejects_the_apply() {
    let n = Net::new("node \"s\" {}\n").await;
    let bad = "node \"s\" {}\nsense \"x\" {\n  node = \"s\"\n  source { timer = { every = \"1s\" } }\n}\nroute \"r\" {\n  from = \"x\"\n  when = \"event.n >\"\n  deliver { mailbox = \"m\" }\n}\n";
    let e = n.hub.apply_cluster(vec![ClusterFile { name: "c.hcl".into(), text: bad.into() }], false, &Addr::root()).await;
    assert!(e.unwrap_err().to_string().contains("route \"r\""));
    assert!(n.hub.inject_event("x", json!({})).is_err(), "the bad version wasn't applied");
}
