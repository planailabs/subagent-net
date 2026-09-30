use super::*;

/// A deterministic stand-in for e5: words (with a few synonyms folded
/// together) hashed into 64 dimensions, plus a shared component so that,
/// like e5, unrelated texts still score around 0.8.
fn fake_embed() -> Embed {
    Arc::new(|texts: &[String], _query: bool| {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 65];
                v[64] = 4.0;
                for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
                    let w = match w {
                        "espresso" | "latte" | "coffee" => "coffee",
                        "tunes" | "music" | "records" => "music",
                        other => other,
                    };
                    let h = w.bytes().fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
                    v[(h % 64) as usize] += 1.0;
                }
                v
            })
            .collect())
    })
}

fn mem() -> Memory {
    Memory::in_memory(None).unwrap()
}

#[test]
fn subjects_parse_and_print() {
    assert_eq!("self".parse::<About>().unwrap(), About::Me);
    assert_eq!("person:Alice".parse::<About>().unwrap(), About::Person("alice".into()));
    assert_eq!(About::Person("bob".into()).to_string(), "person:bob");
    for bad in ["", "person:", "people", "person: "] {
        assert!(bad.parse::<About>().is_err(), "{bad}");
    }
}

#[test]
fn slugs_and_links() {
    assert_eq!(slug("Alice's Coffee!").unwrap(), "alice-s-coffee");
    assert!(slug("?!").is_err());
    assert_eq!(links_in("met [[Alice]] at the [[coffee maker]] and [[alice]] again"), ["alice", "coffee-maker"]);
    assert!(links_in("no [[unclosed").is_empty());
}

#[test]
fn remember_read_update_forget() {
    let m = mem();
    m.remember("Alice", "Likes her coffee black. Met at the [[Coffee maker]].", &About::Person("alice".into())).unwrap();
    m.remember("Coffee maker", "Takes 20 seconds to brew.", &About::World).unwrap();
    let n = m.read("alice").unwrap().unwrap();
    assert_eq!(n.about, "person:alice");
    assert_eq!(n.links, ["Coffee maker"]);
    assert_eq!(m.read("Coffee Maker").unwrap().unwrap().backlinks, ["Alice"]);
    // Rewriting replaces body and links.
    m.remember("Alice", "Prefers tea now.", &About::Person("alice".into())).unwrap();
    let n = m.read("Alice").unwrap().unwrap();
    assert_eq!(n.body, "Prefers tea now.");
    assert!(n.links.is_empty());
    assert!(m.forget("alice").unwrap());
    assert!(!m.forget("alice").unwrap());
    assert!(m.read("alice").unwrap().is_none());
    assert_eq!(m.list(None).unwrap().len(), 1);
}

#[test]
fn links_to_unwritten_memories_show_their_slug() {
    let m = mem();
    m.remember("Plan", "Ask about [[Bob's band]].", &About::Me).unwrap();
    assert_eq!(m.read("plan").unwrap().unwrap().links, ["bob-s-band"]);
}

#[test]
fn bm25_ranks_titles_higher_and_filters_by_subject() {
    let m = mem();
    m.remember("Record player", "Plays vinyl.", &About::World).unwrap();
    m.remember("Evening", "Listened to the record player with Bob.", &About::Person("bob".into())).unwrap();
    m.remember("Mood", "I feel calm.", &About::Me).unwrap();
    let hits = m.recall("record player", None, 5).unwrap();
    assert_eq!(hits[0].title, "Record player", "title matches weigh more");
    assert_eq!(hits.len(), 2);
    let bob = m.recall("record", Some(&About::Person("bob".into())), 5).unwrap();
    assert_eq!(bob.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["Evening"]);
    assert!(m.recall("nothing matches this", None, 5).unwrap().is_empty());
}

#[test]
fn fts_syntax_in_queries_is_harmless() {
    let m = mem();
    m.remember("Quote", "She said \"hi\" (loudly) AND left.", &About::World).unwrap();
    for q in ["\"hi\"", "AND OR NOT", "(loudly", "*", "said: hi -left", ""] {
        m.recall(q, None, 5).unwrap();
    }
    assert_eq!(m.recall("loudly!", None, 5).unwrap()[0].title, "Quote");
}

#[test]
fn dense_recall_finds_paraphrases() {
    let m = Memory::in_memory(Some(fake_embed())).unwrap();
    m.remember("Morning ritual", "Alice always wants an espresso first.", &About::Person("alice".into())).unwrap();
    m.remember("Books", "The shelf has poetry.", &About::World).unwrap();
    // No word overlap with "latte" / "coffee", only the embedding connects them.
    let hits = m.recall("coffee", None, 3).unwrap();
    assert_eq!(hits.first().map(|h| h.title.as_str()), Some("Morning ritual"), "{hits:?}");
    let none = Memory::in_memory(None).unwrap();
    none.remember("Morning ritual", "Alice always wants an espresso first.", &About::Me).unwrap();
    assert!(none.recall("coffee", None, 3).unwrap().is_empty(), "BM25 alone can't");
}

#[test]
fn memories_survive_reopening() {
    let dir = std::env::temp_dir().join(format!("vesper-mem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("m.db");
    let _ = std::fs::remove_file(&path);
    Memory::open(&path, None).unwrap().remember("Me", "My name is Vesper.", &About::Me).unwrap();
    let m = Memory::open(&path, None).unwrap();
    assert_eq!(m.recall("name", None, 1).unwrap()[0].title, "Me");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Real e5 embeddings; needs a download on first run.
#[cfg(feature = "embeddings")]
#[test]
#[ignore = "downloads multilingual-e5-small; run with --ignored"]
fn e5_recall() {
    let cache = std::env::temp_dir().join("vesper-e5-cache");
    let m = Memory::in_memory(Some(e5(&cache).unwrap())).unwrap();
    m.remember("Alice", "Alice drinks her espresso without sugar.", &About::Person("alice".into())).unwrap();
    m.remember("Window", "Rain against the glass most evenings.", &About::World).unwrap();
    assert_eq!(m.recall("how does she take her coffee", None, 1).unwrap()[0].title, "Alice");
}
