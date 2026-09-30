//! A hub (open mode) with a cluster applied, in-process nodes and a scripted LLM.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use subnet::hub::db::ClusterFile;
use subnet::hub::{ConnId, Hub};
use subnet::node::{Node, attach};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::proto::Op;

use super::db_url;
use super::llm::MockLlm;

/// An `agent` block talking to the mock LLM.
pub fn agent(name: &str, system: &str, llm: &str, nodes: &[&str], extra: &str) -> String {
    let nodes = nodes.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ");
    format!(
        "agent {name:?} {{\n  credential {{\n    base_url = {llm:?}\n  }}\n  model = \"mock\"\n  system_prompt = {system:?}\n  nodes = [{nodes}]\n{extra}\n}}\n"
    )
}

pub fn nodes(names: &[&str]) -> String {
    names.iter().map(|n| format!("node {n:?} {{}}\n")).collect()
}

/// Attaches an in-process node to `hub` and waits until it reported what it runs.
pub async fn node_ready(hub: &Arc<Hub>, name: &str) -> ConnId {
    let conn = attach(hub.clone(), Arc::new(Node::new(name, None))).await.unwrap();
    wait_configured(hub, name).await;
    conn
}

pub async fn wait_configured(hub: &Hub, name: &str) {
    for _ in 0..400 {
        if hub.list_nodes().await.iter().any(|n| n.name == name && n.configured) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("node {name} never became ready");
}

pub struct Net {
    pub hub: Arc<Hub>,
    pub llm: MockLlm,
}

impl Net {
    /// `cluster` may use `{llm}` for the mock LLM's URL.
    pub async fn new(cluster: &str) -> Self {
        let llm = MockLlm::start().await;
        let hub = Hub::open(&db_url().await, None).await.unwrap();
        let net = Self { hub, llm };
        net.apply(cluster).await;
        net
    }

    pub async fn apply(&self, cluster: &str) {
        let text = cluster.replace("{llm}", &self.llm.url);
        let files = vec![ClusterFile { name: "test.hcl".into(), text }];
        self.hub.apply_cluster(files, false, &Addr::root()).await.unwrap();
    }

    /// Attaches an in-process node and waits until it reported what it runs.
    pub async fn node(&self, name: &str) -> ConnId {
        node_ready(&self.hub, name).await
    }

    pub async fn spawn(&self, ty: &str, prompt: &str) -> AgentId {
        let v = self.hub.op(&Addr::root(), Op::Spawn { ty: ty.into(), prompt: prompt.into(), tenant: None }).await.unwrap();
        super::id_of(&v)
    }

    pub async fn mail(&self) -> Value {
        let m = self.hub.op(&Addr::root(), Op::WaitInbox { timeout_ms: Some(10_000) }).await.unwrap();
        if m.as_array().unwrap().is_empty() {
            let agents = self.hub.op(&Addr::root(), Op::ListAgents).await.unwrap();
            panic!("no mail within timeout; agents: {agents}");
        }
        m[0].clone()
    }

    pub async fn t(&self, id: AgentId) -> Value {
        self.hub.op(&Addr::root(), Op::Transcript { id }).await.unwrap()
    }

    /// Polls the transcript until `f` holds.
    pub async fn until(&self, id: AgentId, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..400 {
            let t = self.t(id).await;
            if f(&t) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}: {}", self.t(id).await);
    }
}
