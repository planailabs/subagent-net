//! `vesper-memory [DB]`: the memory MCP server on stdio. The database
//! defaults to `vesper-memory.db`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(tracing::Level::WARN).init();
    let db = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "vesper-memory.db".into()));
    vesper_memory::mcp::serve_stdio(&db).await
}
