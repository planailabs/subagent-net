//! The tool router: before a message reaches an agent whose mixture has a
//! `router`, the lazy tools most similar to the message are loaded
//! (`Event::ToolsLoaded`), so the model sees their schemas without asking.
//! Similarity is cosine over embeddings of the message and of each tool's
//! name and description (multilingual e5-small by default, downloaded on
//! first use).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use subnet_cluster::RouterDef;
use subnet_core::chat::ToolDef;

/// Texts to unit vectors; `query` tells queries from passages.
pub trait Embed: Send + Sync {
    fn embed(&self, texts: &[String], query: bool) -> anyhow::Result<Vec<Vec<f32>>>;
}

type Make = Box<dyn Fn() -> anyhow::Result<Arc<dyn Embed>> + Send + Sync>;

pub struct Router {
    make: Mutex<Option<Make>>,
    embed: OnceLock<Option<Arc<dyn Embed>>>,
    /// Tool text → embedding.
    cache: Mutex<HashMap<String, Vec<f32>>>,
}

impl Router {
    /// A router whose embedder is made on first use.
    pub fn new(make: Make) -> Self {
        Router { make: Mutex::new(Some(make)), embed: OnceLock::new(), cache: Mutex::default() }
    }

    /// The default: e5 in `$SUBNET_MODELS` (or `subnet-models/`), if built
    /// with the `router` feature.
    pub fn default_e5() -> Self {
        Router::new(Box::new(|| {
            #[cfg(feature = "router")]
            {
                let dir = std::env::var_os("SUBNET_MODELS").map(std::path::PathBuf::from).unwrap_or_else(|| "subnet-models".into());
                Ok(Arc::new(E5::new(&dir)?) as Arc<dyn Embed>)
            }
            #[cfg(not(feature = "router"))]
            anyhow::bail!("subnet was built without the router feature")
        }))
    }

    /// Replaces the embedder (tests, other models).
    pub fn with(embed: Arc<dyn Embed>) -> Self {
        let r = Router::new(Box::new(|| anyhow::bail!("unused")));
        let _ = r.embed.set(Some(embed));
        r
    }

    fn embedder(&self) -> Option<Arc<dyn Embed>> {
        self.embed
            .get_or_init(|| {
                let make = self.make.lock().unwrap().take()?;
                match make() {
                    Ok(e) => Some(e),
                    Err(e) => {
                        tracing::warn!(error = %e, "tool router unavailable: no tools are pre-loaded");
                        None
                    }
                }
            })
            .clone()
    }

    /// The names of up to `top_k` of `tools` that match `text`, best first.
    pub async fn pick(self: &Arc<Self>, def: &RouterDef, tools: Vec<ToolDef>, text: String) -> Vec<String> {
        if tools.is_empty() || text.trim().is_empty() {
            return vec![];
        }
        let me = self.clone();
        let (top_k, min) = (def.top_k, def.min_score);
        let r = tokio::task::spawn_blocking(move || me.rank(&tools, &text).map(|v| v.into_iter().filter(|(s, _)| *s >= min).take(top_k).map(|(_, n)| n).collect()));
        match r.await {
            Ok(Ok(names)) => names,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "tool router failed: nothing pre-loaded");
                vec![]
            }
            Err(e) => {
                tracing::warn!(error = %e, "tool router panicked: nothing pre-loaded");
                vec![]
            }
        }
    }

    /// Every tool with its similarity to `text`, best first.
    fn rank(&self, tools: &[ToolDef], text: &str) -> anyhow::Result<Vec<(f32, String)>> {
        let Some(e) = self.embedder() else { return Ok(vec![]) };
        let docs: Vec<String> = tools.iter().map(|t| format!("{}: {}", t.name.replace(['.', '_'], " "), t.description)).collect();
        let missing: Vec<String> = {
            let c = self.cache.lock().unwrap();
            docs.iter().filter(|d| !c.contains_key(*d)).cloned().collect()
        };
        if !missing.is_empty() {
            let vs = e.embed(&missing, false)?;
            self.cache.lock().unwrap().extend(missing.into_iter().zip(vs));
        }
        let q = e.embed(&[text.to_string()], true)?.remove(0);
        let c = self.cache.lock().unwrap();
        let mut scored: Vec<(f32, String)> = tools.iter().zip(&docs).map(|(t, d)| (cosine(&q, &c[d]), t.name.clone())).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(scored)
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let n = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (n(a) * n(b)).max(1e-9)
}

#[cfg(feature = "router")]
struct E5(Mutex<fastembed::TextEmbedding>);

#[cfg(feature = "router")]
impl E5 {
    fn new(cache: &std::path::Path) -> anyhow::Result<Self> {
        use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
        let m = TextEmbedding::try_new(TextInitOptions::new(EmbeddingModel::MultilingualE5Small).with_cache_dir(cache.to_path_buf()))?;
        Ok(E5(Mutex::new(m)))
    }
}

#[cfg(feature = "router")]
impl Embed for E5 {
    fn embed(&self, texts: &[String], query: bool) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefix = if query { "query: " } else { "passage: " };
        let texts: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        Ok(self.0.lock().unwrap().embed(texts, None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words hashed into a few dimensions, synonyms folded.
    pub struct Words;
    impl Embed for Words {
        fn embed(&self, texts: &[String], _: bool) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; 32];
                    for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
                        let w = if matches!(w, "web" | "internet" | "online" | "search") { "web" } else { w };
                        v[w.bytes().fold(5usize, |h, b| h.wrapping_mul(33) ^ b as usize) % 32] += 1.0;
                    }
                    v
                })
                .collect())
        }
    }

    fn tool(name: &str, d: &str) -> ToolDef {
        ToolDef { name: name.into(), description: d.into(), parameters: serde_json::json!({}) }
    }

    #[tokio::test]
    async fn picks_the_closest_tools() {
        let r = Arc::new(Router::with(Arc::new(Words)));
        let tools = vec![tool("web.search", "Search the web."), tool("cal.add", "Add a calendar event."), tool("web.scrape", "Read a web page.")];
        let def = RouterDef { top_k: 1, min_score: 0.1 };
        assert_eq!(r.pick(&def, tools.clone(), "look it up online".into()).await, ["web.search"]);
        let def = RouterDef { top_k: 3, min_score: 0.99 };
        assert!(r.pick(&def, tools.clone(), "look it up online".into()).await.is_empty(), "nothing is that close");
        assert!(r.pick(&def, vec![], "x".into()).await.is_empty());
        assert_eq!(r.cache.lock().unwrap().len(), 3, "tools are embedded once");
    }

    /// The real model, with the default threshold.
    #[cfg(feature = "router")]
    #[tokio::test]
    #[ignore = "downloads multilingual-e5-small"]
    async fn e5_routes_by_meaning() {
        let dir = std::env::temp_dir().join("subnet-router-e5");
        let r = Arc::new(Router::with(Arc::new(E5::new(&dir).unwrap())));
        let tools = vec![
            tool("web.firecrawl_search", "Search the web and optionally extract content from search results."),
            tool("web.firecrawl_scrape", "Scrape content from a single URL with advanced options."),
            tool("cal.add_event", "Add an event to the user's calendar."),
        ];
        let def = RouterDef { top_k: 2, min_score: 0.78 };
        let picked = r.pick(&def, tools.clone(), "what's the latest news about the James Webb telescope?".into()).await;
        assert_eq!(picked.first().map(String::as_str), Some("web.firecrawl_search"), "{picked:?}");
        let picked = r.pick(&def, tools, "put dinner with Sam on Friday at 7 in my calendar".into()).await;
        assert_eq!(picked.first().map(String::as_str), Some("cal.add_event"), "{picked:?}");
    }

    #[tokio::test]
    async fn a_router_without_a_model_picks_nothing() {
        let r = Arc::new(Router::new(Box::new(|| anyhow::bail!("no model here"))));
        let def = RouterDef { top_k: 3, min_score: 0.0 };
        assert!(r.pick(&def, vec![tool("a.b", "c")], "c".into()).await.is_empty());
    }
}
