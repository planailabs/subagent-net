//! Test support: a fresh Postgres database per test.
//!
//! Uses `$DATABASE_URL` (a server where we may create databases) if set;
//! otherwise starts a throwaway cluster under cargo's target tmp dir with the
//! `initdb`/`pg_ctl` binaries from the devshell. That cluster is reused across
//! runs; stop it with `pg_ctl -D target/tmp/testpg stop`.
// Shelling out is deliberate: there is no Rust binding for running a Postgres server.
#![allow(dead_code)]

pub mod llm;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use subnet_core::proto::ToSpawner;
use tokio::sync::mpsc::UnboundedReceiver;

fn server_url() -> String {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        if let Ok(u) = std::env::var("DATABASE_URL") {
            return u;
        }
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testpg");
        let port_file = dir.join("subnet-port");
        if !dir.join("PG_VERSION").exists() {
            let _ = std::fs::remove_dir_all(&dir);
            let ok = Command::new("initdb")
                .args(["-U", "postgres", "--auth=trust", "-D"])
                .arg(&dir)
                .stdout(std::process::Stdio::null())
                .status()
                .expect("initdb not found; run tests inside `nix develop` or set DATABASE_URL")
                .success();
            assert!(ok, "initdb failed");
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            std::fs::write(&port_file, port.to_string()).unwrap();
        }
        let port = std::fs::read_to_string(&port_file).unwrap();
        let running = Command::new("pg_ctl").arg("status").arg("-D").arg(&dir).status().unwrap().success();
        if !running {
            let log = dir.join("log.txt");
            let ok = Command::new("pg_ctl")
                .arg("-D")
                .arg(&dir)
                .arg("-l")
                .arg(&log)
                .args(["-w", "-o"])
                .arg(format!("-p {port} -h 127.0.0.1 -k '' -c max_connections=500 -c fsync=off"))
                .arg("start")
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "pg_ctl start failed, see {}", log.display());
        }
        format!("postgres://postgres@127.0.0.1:{port}/postgres")
    })
    .clone()
}

/// URL of a new, empty database.
pub async fn db_url() -> String {
    let base = server_url();
    let name = format!("t_{}", uuid::Uuid::new_v4().simple());
    let pool = sqlx::PgPool::connect(&base).await.expect("connect to test postgres");
    // `name` is generated above, not user input.
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {name}"))).execute(&pool).await.unwrap();
    pool.close().await;
    let (prefix, _) = base.rsplit_once('/').unwrap();
    format!("{prefix}/{name}")
}

pub async fn recv(rx: &mut UnboundedReceiver<ToSpawner>) -> ToSpawner {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("timed out waiting for hub").expect("channel closed")
}

/// Asserts nothing arrives for a short while.
pub async fn quiet(rx: &mut UnboundedReceiver<ToSpawner>) {
    if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        panic!("unexpected message {m:?}");
    }
}

pub fn id_of(v: &Value) -> uuid::Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}
