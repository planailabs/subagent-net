//! The two store backends keep the same schema.

mod common;

use std::collections::BTreeMap;

use subnet::hub::db::{Db, Pool};

/// Every table's columns (sqlx's own bookkeeping left out).
async fn schema(db: &Db) -> BTreeMap<String, Vec<String>> {
    let rows: Vec<(String, String)> = match &db.pool {
        Pool::Pg(p) => sqlx::query_as("select table_name::text, column_name::text from information_schema.columns where table_schema = 'public' order by table_name, column_name")
            .fetch_all(p)
            .await
            .unwrap(),
        Pool::Lite(p) => sqlx::query_as("select m.name, c.name from sqlite_master m join pragma_table_info(m.name) c where m.type = 'table' order by m.name, c.name")
            .fetch_all(p)
            .await
            .unwrap(),
    };
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (t, c) in rows.into_iter().filter(|(t, _)| !t.starts_with("_sqlx") && t != "sqlite_sequence") {
        out.entry(t).or_default().push(c);
    }
    out
}

#[tokio::test]
async fn postgres_and_sqlite_have_the_same_tables_and_columns() {
    let pg = Db::connect(&common::pg_url().await).await.unwrap();
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testlite");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("schema_{}.db", uuid::Uuid::new_v4().simple()));
    let lite = Db::connect(&format!("sqlite://{}", file.display())).await.unwrap();
    let (a, b) = (schema(&pg).await, schema(&lite).await);
    assert!(a.contains_key("events") && a.len() >= 12, "{a:?}");
    assert_eq!(a, b, "migrations/postgres and migrations/sqlite drifted apart");
    let _ = std::fs::remove_file(file);
}

#[tokio::test]
async fn a_second_hub_on_the_same_sqlite_file_stands_by() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testlite");
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}", dir.join(format!("standby_{}.db", uuid::Uuid::new_v4().simple())).display());
    let a = subnet::hub::Hub::start(&url, None, "http://a").await.unwrap();
    a.wait_leader().await;
    let b = subnet::hub::Hub::start(&url, None, "http://b").await.unwrap();
    for _ in 0..50 {
        if b.leader_url().as_deref() == Some("http://a") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(b.leader_url().as_deref(), Some("http://a"));
    assert!(!b.is_leader(), "the lock file keeps b waiting");
    a.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(10), b.wait_leader()).await.expect("b takes over");
}
