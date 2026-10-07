//! Test support: a fresh Postgres database per test.
//!
//! Uses `$DATABASE_URL` (a server where we may create databases) if set;
//! otherwise starts a throwaway cluster under cargo's target tmp dir with the
//! `initdb`/`pg_ctl` binaries from the devshell. That cluster is reused across
//! runs; stop it with `pg_ctl -D target/tmp/testpg stop`.
// Shelling out is deliberate: there is no Rust binding for running a Postgres server.
#![allow(dead_code)]

pub mod llm;
pub mod net;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use subnet::wire::ToNode;
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

/// Drops test databases left by earlier test processes (once per process), so
/// the throwaway cluster doesn't grow without bound. (Cargo runs test binaries
/// one after another; two concurrent `cargo test`s would sweep each other.)
async fn sweep(pool: &sqlx::PgPool) {
    static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEPT
        .get_or_init(|| async {
            let mine = format!("t_{}_", std::process::id());
            let rows: Vec<(String,)> = sqlx::query_as("select datname from pg_database where datname like 't\\_%'")
                .fetch_all(pool)
                .await
                .unwrap_or_default();
            for (name,) in rows.into_iter().filter(|(n,)| !n.starts_with(&mine)) {
                // Names are ours (t_<pid>_<uuid>), not user input.
                let _ = sqlx::query(sqlx::AssertSqlSafe(format!("drop database if exists {name} with (force)"))).execute(pool).await;
            }
        })
        .await;
}

/// Whether the tests run on SQLite (`SUBNET_TEST_DB=sqlite`) instead of Postgres.
pub fn sqlite() -> bool {
    std::env::var("SUBNET_TEST_DB").is_ok_and(|v| v == "sqlite")
}

/// URL of a new, empty database: Postgres, or with `SUBNET_TEST_DB=sqlite` a
/// fresh SQLite file under cargo's target tmp dir.
pub async fn db_url() -> String {
    if sqlite() {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testlite");
        std::fs::create_dir_all(&dir).unwrap();
        // Files of earlier runs go once per process.
        static SWEPT: OnceLock<()> = OnceLock::new();
        SWEPT.get_or_init(|| {
            let mine = format!("t_{}_", std::process::id());
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                if !e.file_name().to_string_lossy().starts_with(&mine) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        });
        let file = dir.join(format!("t_{}_{}.db", std::process::id(), uuid::Uuid::new_v4().simple()));
        return format!("sqlite://{}", file.display());
    }
    pg_url().await
}

/// URL of a new, empty Postgres database (whatever `SUBNET_TEST_DB` says).
pub async fn pg_url() -> String {
    let base = server_url();
    let name = format!("t_{}_{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let pool = sqlx::PgPool::connect(&base).await.expect("connect to test postgres");
    sweep(&pool).await;
    // `name` is generated above, not user input.
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {name}"))).execute(&pool).await.unwrap();
    pool.close().await;
    let (prefix, _) = base.rsplit_once('/').unwrap();
    format!("{prefix}/{name}")
}

pub async fn recv(rx: &mut UnboundedReceiver<ToNode>) -> ToNode {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for hub")
        .expect("channel closed")
}

/// Asserts nothing arrives for a short while.
pub async fn quiet(rx: &mut UnboundedReceiver<ToNode>) {
    if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        panic!("unexpected message {m:?}");
    }
}

pub fn id_of(v: &Value) -> uuid::Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

/// Path of one of this crate's examples, building it if a narrower `cargo
/// test` invocation didn't. (Shelling out to cargo is the point here.)
pub fn example(name: &str) -> String {
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_subnet"));
    let p = bin.parent().unwrap().join("examples").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !p.exists() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        if !p.exists() {
            let ok = Command::new(env!("CARGO"))
                .args(["build", "-q", "-p", "subnet", "--example", name])
                .status()
                .expect("running cargo")
                .success();
            assert!(ok, "building example {name} failed");
        }
    }
    p.to_string_lossy().into_owned()
}
