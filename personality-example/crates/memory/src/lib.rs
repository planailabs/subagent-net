//! Vesper's memory: a Rust port of self-learning-model's store. Notes with
//! `[[wikilinks]]`, an FTS5 index ranked by BM25 (titles weigh more), and
//! optional dense embeddings; recall fuses both rankings. Every memory is
//! about someone: `self`, `person:<name>` or `world`.

pub mod mcp;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Title weight for BM25 (title vs body).
const TITLE_WEIGHT: f64 = 5.0;
/// Reciprocal-rank-fusion constant.
const RRF_K: f64 = 60.0;
/// Cosine floor for a dense hit (e5 scores are compressed into a high band).
pub const DENSE_MIN: f32 = 0.80;

/// Turns texts into vectors. `query` distinguishes queries from stored
/// passages (e5 wants different prefixes).
pub type Embed = Arc<dyn Fn(&[String], bool) -> anyhow::Result<Vec<Vec<f32>>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum About {
    /// Vesper herself.
    Me,
    Person(String),
    World,
}

impl std::fmt::Display for About {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            About::Me => f.write_str("self"),
            About::Person(n) => write!(f, "person:{n}"),
            About::World => f.write_str("world"),
        }
    }
}

impl std::str::FromStr for About {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "self" => Ok(About::Me),
            "world" => Ok(About::World),
            p => match p.strip_prefix("person:").map(str::trim) {
                Some(n) if !n.is_empty() => Ok(About::Person(n.to_lowercase())),
                _ => Err(format!("about must be self, world or person:<name>, not {s:?}")),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub title: String,
    pub body: String,
    pub about: String,
    /// Unix seconds.
    pub updated: i64,
    /// Titles this note links to.
    pub links: Vec<String>,
    /// Titles linking here.
    pub backlinks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub title: String,
    pub body: String,
    pub about: String,
    /// Fused score (higher is better).
    pub score: f64,
    pub links: Vec<String>,
}

pub fn slug(title: &str) -> Result<String, String> {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { Err(format!("title {title:?} has no usable characters")) } else { Ok(s) }
}

/// `[[target]]` references in a body, as slugs, first occurrence first.
pub fn links_in(body: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = body;
    while let Some(i) = rest.find("[[") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("]]") else { break };
        if let Ok(s) = slug(&after[..j])
            && !out.contains(&s)
        {
            out.push(s);
        }
        rest = &after[j + 2..];
    }
    out
}

pub struct Memory {
    db: Mutex<Connection>,
    embed: Option<Embed>,
}

const SCHEMA: &str = "
create table if not exists memories (
    id integer primary key,
    slug text not null unique,
    title text not null,
    body text not null,
    about text not null,
    created integer not null,
    updated integer not null,
    embedding blob
);
create table if not exists links (
    src text not null,
    dst text not null,
    primary key (src, dst)
);
create virtual table if not exists memories_fts using fts5(title, body, content='memories', content_rowid='id');
create trigger if not exists memories_ai after insert on memories begin
    insert into memories_fts(rowid, title, body) values (new.id, new.title, new.body);
end;
create trigger if not exists memories_ad after delete on memories begin
    insert into memories_fts(memories_fts, rowid, title, body) values ('delete', old.id, old.title, old.body);
end;
create trigger if not exists memories_au after update on memories begin
    insert into memories_fts(memories_fts, rowid, title, body) values ('delete', old.id, old.title, old.body);
    insert into memories_fts(rowid, title, body) values (new.id, new.title, new.body);
end;
";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { d / (na.sqrt() * nb.sqrt()) }
}

/// A user's query as an FTS5 expression: its words, OR-ed and quoted, so
/// punctuation can't be read as FTS syntax.
fn fts_query(q: &str) -> Option<String> {
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
        .map(|w| format!("\"{}\"", w.to_lowercase()))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

impl Memory {
    pub fn open(path: &Path, embed: Option<Embed>) -> anyhow::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db), embed })
    }

    pub fn in_memory(embed: Option<Embed>) -> anyhow::Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db), embed })
    }

    /// Writes (or rewrites) a memory. Returns its title.
    pub fn remember(&self, title: &str, body: &str, about: &About) -> anyhow::Result<String> {
        let s = slug(title).map_err(anyhow::Error::msg)?;
        let body = body.trim();
        let embedding = match &self.embed {
            Some(e) => Some(to_blob(&e(&[format!("{title}. {body}")], false)?.remove(0))),
            None => None,
        };
        let links = links_in(body);
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let t = now();
        tx.execute(
            "insert into memories (slug, title, body, about, created, updated, embedding) values (?1, ?2, ?3, ?4, ?5, ?5, ?6)
             on conflict (slug) do update set title = ?2, body = ?3, about = ?4, updated = ?5, embedding = ?6",
            params![s, title.trim(), body, about.to_string(), t, embedding],
        )?;
        tx.execute("delete from links where src = ?1", params![s])?;
        for l in links.iter().filter(|l| **l != s) {
            tx.execute("insert or ignore into links (src, dst) values (?1, ?2)", params![s, l])?;
        }
        tx.commit()?;
        Ok(title.trim().to_string())
    }

    fn title_of(db: &Connection, slug: &str) -> Option<String> {
        db.query_row("select title from memories where slug = ?1", params![slug], |r| r.get(0)).optional().ok().flatten()
    }

    fn linked(db: &Connection, slug: &str) -> (Vec<String>, Vec<String>) {
        let q = |sql: &str| -> Vec<String> {
            let mut st = db.prepare(sql).unwrap();
            let rows: Vec<String> = st.query_map(params![slug], |r| r.get::<_, String>(0)).unwrap().filter_map(Result::ok).collect();
            // Links may point at memories not written yet: show their slug then.
            rows.into_iter().map(|s| Self::title_of(db, &s).unwrap_or(s)).collect()
        };
        (q("select dst from links where src = ?1 order by dst"), q("select src from links where dst = ?1 order by src"))
    }

    pub fn read(&self, title: &str) -> anyhow::Result<Option<Note>> {
        let s = slug(title).map_err(anyhow::Error::msg)?;
        let db = self.db.lock().unwrap();
        let row = db
            .query_row("select title, body, about, updated from memories where slug = ?1", params![s], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
            })
            .optional()?;
        Ok(row.map(|(title, body, about, updated)| {
            let (links, backlinks) = Self::linked(&db, &s);
            Note { title, body, about, updated, links, backlinks }
        }))
    }

    pub fn forget(&self, title: &str) -> anyhow::Result<bool> {
        let s = slug(title).map_err(anyhow::Error::msg)?;
        let db = self.db.lock().unwrap();
        let n = db.execute("delete from memories where slug = ?1", params![s])?;
        db.execute("delete from links where src = ?1", params![s])?;
        Ok(n > 0)
    }

    /// Titles, newest first, optionally only about one subject.
    pub fn list(&self, about: Option<&About>) -> anyhow::Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("select title, about from memories where ?1 is null or about = ?1 order by updated desc, id desc")?;
        let rows = st
            .query_map(params![about.map(|a| a.to_string())], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Hybrid search: BM25 over titles and bodies, fused with embedding
    /// similarity when embeddings are on.
    pub fn recall(&self, query: &str, about: Option<&About>, k: usize) -> anyhow::Result<Vec<Hit>> {
        let about = about.map(|a| a.to_string());
        let qvec = match &self.embed {
            Some(e) => Some(e(&[query.to_string()], true)?.remove(0)),
            None => None,
        };
        let db = self.db.lock().unwrap();
        let mut ranks: HashMap<i64, f64> = HashMap::new();
        if let Some(fq) = fts_query(query) {
            let mut st = db.prepare(
                "select m.id from memories_fts f join memories m on m.id = f.rowid
                 where memories_fts match ?1 and (?2 is null or m.about = ?2)
                 order by bm25(memories_fts, ?3, 1.0) limit 50",
            )?;
            let ids = st.query_map(params![fq, about, TITLE_WEIGHT], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
            for (rank, id) in ids.into_iter().enumerate() {
                *ranks.entry(id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }
        if let Some(q) = &qvec {
            let mut st = db.prepare("select id, embedding from memories where embedding is not null and (?1 is null or about = ?1)")?;
            let mut scored: Vec<(i64, f32)> = st
                .query_map(params![about], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
                .filter_map(Result::ok)
                .map(|(id, b)| (id, cosine(q, &from_blob(&b))))
                .filter(|(_, c)| *c >= DENSE_MIN)
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            for (rank, (id, _)) in scored.into_iter().take(50).enumerate() {
                *ranks.entry(id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            }
        }
        let mut ranked: Vec<(i64, f64)> = ranks.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(k.max(1));
        ranked
            .into_iter()
            .map(|(id, score)| {
                let (slug, title, body, about): (String, String, String, String) =
                    db.query_row("select slug, title, body, about from memories where id = ?1", params![id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?;
                let (links, _) = Self::linked(&db, &slug);
                Ok(Hit { title, body, about, score, links })
            })
            .collect()
    }
}

/// multilingual-e5-small via fastembed (downloaded to the cache dir on first use).
#[cfg(feature = "embeddings")]
pub fn e5(cache: &Path) -> anyhow::Result<Embed> {
    use fastembed::{EmbeddingModel, TextInitOptions, TextEmbedding};
    let model = TextEmbedding::try_new(TextInitOptions::new(EmbeddingModel::MultilingualE5Small).with_cache_dir(cache.to_path_buf()))?;
    let model = Mutex::new(model);
    Ok(Arc::new(move |texts: &[String], query: bool| {
        let prefix = if query { "query: " } else { "passage: " };
        let texts: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        Ok(model.lock().unwrap().embed(texts, None)?)
    }))
}

#[cfg(test)]
mod tests;
