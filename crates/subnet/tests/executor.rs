//! External executors: a process replaces the LLM call.

mod common;

use common::net::{Net, nodes};
use subnet_core::addr::Addr;
use subnet_core::agent::PauseMode;
use subnet_core::proto::Op;

fn executor() -> String {
    common::example("echo_executor")
}

async fn net() -> Net {
    let cluster = format!(
        "{}agent \"bot\" {{\n  credential {{\n    base_url = \"http://unused\"\n  }}\n  model = \"none\"\n  executor {{ command = [{:?}] }}\n  nodes = [\"s\"]\n}}\n",
        nodes(&["s"]),
        executor()
    );
    let n = Net::new(&cluster).await;
    n.node("s").await;
    n
}

#[tokio::test]
async fn external_executor_answers() {
    let n = net().await;
    n.spawn("bot", "hi there").await;
    assert_eq!(n.mail().await["content"], "bot external: hi there");
}

#[tokio::test]
async fn external_executor_tool_calls_run_on_the_node() {
    let n = net().await;
    n.spawn("bot", "tool please").await;
    let c = n.mail().await["content"].as_str().unwrap().to_string();
    assert!(c.starts_with("tool said: "), "{c}");
    assert!(c.contains("\"bot\""), "list_types ran: {c}");
}

#[tokio::test]
async fn hard_pause_aborts_external_thinking_and_keeps_partial() {
    let n = net().await;
    let id = n.spawn("bot", "slow").await;
    n.until(id, "partial", |t| t["partial"]["content"].as_str().is_some_and(|c| c.len() >= 4)).await;
    n.hub.op(&Addr::root(), Op::Pause { id, mode: PauseMode::Hard, tree: false }).await.unwrap();
    let t = n.until(id, "paused", |t| t["paused"] == true).await;
    let partial = t["partial"]["content"].as_str().unwrap().to_string();
    assert!(partial.starts_with("one "), "{partial}");
    assert!(!partial.ends_with("eight"), "{partial}");
}

#[tokio::test]
async fn crashed_executor_fails_the_turn_and_restarts() {
    let n = net().await;
    let id = n.spawn("bot", "crash").await;
    let m = n.mail().await;
    assert_eq!(m["status"], "failed");
    assert!(m["content"].as_str().unwrap().contains("executor exited"), "{m}");
    // The next request starts a fresh process.
    n.hub.op(&Addr::root(), Op::Send { to: Addr::Agent(id), content: "again".into() }).await.unwrap();
    n.hub.op(&Addr::root(), Op::Resume { id, tree: false }).await.unwrap();
    let m = n.mail().await;
    assert_eq!(m["status"], "idle", "{m}");
}
