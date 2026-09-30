//! The memory as an MCP server: `remember`, `recall`, `read`, `forget`, `list`.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use crate::{About, Memory};

#[derive(Clone)]
pub struct MemoryServer(pub Arc<Memory>);

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Remember {
    /// A short, unique title; writing the same title again replaces the memory.
    pub title: String,
    /// The memory. `[[Other title]]` links to another memory.
    pub body: String,
    /// Who it's about: `self`, `person:<name>` or `world`.
    pub about: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Recall {
    /// What to look for, in plain words.
    pub query: String,
    /// Only memories about `self`, `person:<name>` or `world`.
    pub about: Option<String>,
    /// How many (default 5).
    pub k: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Title {
    pub title: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct List {
    /// Only memories about `self`, `person:<name>` or `world`.
    pub about: Option<String>,
}

fn about(s: Option<&str>) -> Result<Option<About>, String> {
    s.map(str::parse).transpose()
}

fn json(v: impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(&v).map_err(|e| e.to_string())
}

#[tool_router(server_handler)]
impl MemoryServer {
    #[tool(description = "Write down a memory about yourself (about: self), a person (about: person:<name>) or the world (about: world). Same title replaces it.")]
    fn remember(&self, Parameters(r): Parameters<Remember>) -> Result<String, String> {
        let about: About = r.about.parse()?;
        let title = self.0.remember(&r.title, &r.body, &about).map_err(|e| e.to_string())?;
        Ok(format!("remembered {title:?} about {about}"))
    }

    #[tool(description = "Search memories by meaning and words. Returns the best matches with their links.")]
    fn recall(&self, Parameters(r): Parameters<Recall>) -> Result<String, String> {
        let about = about(r.about.as_deref())?;
        json(self.0.recall(&r.query, about.as_ref(), r.k.unwrap_or(5).clamp(1, 50)).map_err(|e| e.to_string())?)
    }

    #[tool(description = "Read one memory by title, with what it links to and what links to it.")]
    fn read(&self, Parameters(Title { title }): Parameters<Title>) -> Result<String, String> {
        match self.0.read(&title).map_err(|e| e.to_string())? {
            Some(n) => json(n),
            None => Err(format!("no memory titled {title:?}")),
        }
    }

    #[tool(description = "Forget a memory by title.")]
    fn forget(&self, Parameters(Title { title }): Parameters<Title>) -> Result<String, String> {
        match self.0.forget(&title).map_err(|e| e.to_string())? {
            true => Ok(format!("forgot {title:?}")),
            false => Err(format!("no memory titled {title:?}")),
        }
    }

    #[tool(description = "List memory titles, optionally only those about self, person:<name> or world.")]
    fn list(&self, Parameters(List { about: a }): Parameters<List>) -> Result<String, String> {
        let about = about(a.as_deref())?;
        let rows = self.0.list(about.as_ref()).map_err(|e| e.to_string())?;
        json(rows.into_iter().map(|(title, about)| serde_json::json!({"title": title, "about": about})).collect::<Vec<_>>())
    }
}

/// Serves the memory in `db` on stdio. `SUBJECT_MEMORY_EMBEDDINGS=off`
/// disables dense retrieval (BM25 only); the e5 model is cached in
/// `models/` next to the database.
pub async fn serve_stdio(db: &std::path::Path) -> anyhow::Result<()> {
    use rmcp::{ServiceExt, transport::stdio};
    let embed = match std::env::var("SUBJECT_MEMORY_EMBEDDINGS").as_deref() {
        Ok("off") => None,
        _ => embeddings(db)?,
    };
    let memory = Memory::open(db, embed)?;
    MemoryServer(Arc::new(memory)).serve(stdio()).await?.waiting().await?;
    Ok(())
}

#[cfg(feature = "embeddings")]
fn embeddings(db: &std::path::Path) -> anyhow::Result<Option<crate::Embed>> {
    let cache = db.parent().unwrap_or(std::path::Path::new(".")).join("models");
    Ok(Some(crate::e5(&cache)?))
}

#[cfg(not(feature = "embeddings"))]
fn embeddings(_: &std::path::Path) -> anyhow::Result<Option<crate::Embed>> {
    tracing::warn!("built without the embeddings feature: BM25 only");
    Ok(None)
}
