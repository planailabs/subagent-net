//! The `vesper-memory` binary as a stdio MCP server.

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};

async fn call(mcp: &rmcp::service::RunningService<rmcp::RoleClient, ()>, tool: &'static str, args: Value) -> (bool, String) {
    let mut p = CallToolRequestParams::new(tool);
    p.arguments = args.as_object().cloned();
    let r = mcp.call_tool(p).await.unwrap();
    (r.is_error == Some(true), r.content[0].as_text().unwrap().text.clone())
}

#[tokio::test]
async fn tools_over_stdio() {
    let dir = std::env::temp_dir().join(format!("vesper-mem-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("m.db");
    let t = TokioChildProcess::new(tokio::process::Command::new(env!("CARGO_BIN_EXE_vesper-memory")).configure(|c| {
        c.arg(&db).env("SUBJECT_MEMORY_EMBEDDINGS", "off").kill_on_drop(true);
    }))
    .unwrap();
    let mcp = ().serve(t).await.unwrap();
    let mut tools: Vec<String> = mcp.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    tools.sort();
    assert_eq!(tools, ["forget", "list", "read", "recall", "remember"]);

    let (err, _) = call(&mcp, "remember", json!({"title": "Alice", "body": "Black coffee, no sugar. Likes [[Record player]].", "about": "person:Alice"})).await;
    assert!(!err);
    call(&mcp, "remember", json!({"title": "Record player", "body": "Plays vinyl.", "about": "world"})).await;
    let (err, msg) = call(&mcp, "remember", json!({"title": "X", "body": "y", "about": "everyone"})).await;
    assert!(err && msg.contains("person:<name>"), "{msg}");

    let (_, hits) = call(&mcp, "recall", json!({"query": "coffee sugar", "about": "person:alice"})).await;
    let hits: Value = serde_json::from_str(&hits).unwrap();
    assert_eq!(hits[0]["title"], "Alice");
    assert_eq!(hits[0]["links"], json!(["Record player"]));

    let (_, note) = call(&mcp, "read", json!({"title": "record player"})).await;
    assert_eq!(serde_json::from_str::<Value>(&note).unwrap()["backlinks"], json!(["Alice"]));
    let (_, list) = call(&mcp, "list", json!({"about": "world"})).await;
    assert_eq!(serde_json::from_str::<Value>(&list).unwrap(), json!([{"title": "Record player", "about": "world"}]));
    assert!(!call(&mcp, "forget", json!({"title": "Alice"})).await.0);
    assert!(call(&mcp, "read", json!({"title": "Alice"})).await.0);
    mcp.cancel().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
