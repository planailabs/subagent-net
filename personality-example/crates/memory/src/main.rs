//! `vesper-memory [DB]`: the memory MCP server on stdio. The database
//! defaults to `vesper-memory.db`; `SUBJECT_MEMORY_EMBEDDINGS=off` disables
//! dense retrieval (BM25 only).

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};
use vesper_memory::{Memory, mcp::MemoryServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(tracing::Level::WARN).init();
    let db = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "vesper-memory.db".into()));
    let embed = match std::env::var("SUBJECT_MEMORY_EMBEDDINGS").as_deref() {
        Ok("off") => None,
        _ => embeddings(&db)?,
    };
    let memory = Memory::open(&db, embed)?;
    MemoryServer(Arc::new(memory)).serve(stdio()).await?.waiting().await?;
    Ok(())
}

#[cfg(feature = "embeddings")]
fn embeddings(db: &std::path::Path) -> anyhow::Result<Option<vesper_memory::Embed>> {
    let cache = db.parent().unwrap_or(std::path::Path::new(".")).join("models");
    Ok(Some(vesper_memory::e5(&cache)?))
}

#[cfg(not(feature = "embeddings"))]
fn embeddings(_: &std::path::Path) -> anyhow::Result<Option<vesper_memory::Embed>> {
    tracing::warn!("built without the embeddings feature: BM25 only");
    Ok(None)
}
