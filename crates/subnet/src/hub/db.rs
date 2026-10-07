//! The hub's persistence: Postgres or SQLite, chosen by the database URL
//! (`postgres://…`, `sqlite://…`). Schema changes go through
//! `migrations/postgres` and `migrations/sqlite`, kept equivalent.

use std::str::FromStr;

use sqlx::{PgPool, Row, SqlitePool, postgres::PgPoolOptions, sqlite::SqliteConnectOptions, sqlite::SqlitePoolOptions, types::Json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Event, Spec};
use subnet_core::proto::Mail;

pub enum Pool {
    Pg(PgPool),
    Lite(SqlitePool),
}

/// Runs `$body` with `$p` bound to the pool, whichever backend it is: for
/// queries that are the same in both. With two SQL texts, `$q` is the one
/// for the backend.
macro_rules! on {
    ($db:expr, |$p:ident| $body:expr) => {
        match &$db.pool {
            Pool::Pg($p) => $body,
            Pool::Lite($p) => $body,
        }
    };
    ($db:expr, |$p:ident, $q:ident| $pg:expr, $lite:expr => $body:expr) => {
        match &$db.pool {
            Pool::Pg($p) => {
                let $q = $pg;
                $body
            }
            Pool::Lite($p) => {
                let $q = $lite;
                $body
            }
        }
    };
}

pub struct Db {
    pub pool: Pool,
    /// This hub's leader term (0 = not leading); event appends check it.
    pub term: std::sync::atomic::AtomicI64,
}

pub struct AgentRow {
    pub id: AgentId,
    pub spec: Spec,
    pub epoch: u64,
}

impl Db {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = if url.starts_with("sqlite:") {
            let opts = SqliteConnectOptions::from_str(url)?
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                .foreign_keys(true)
                .busy_timeout(std::time::Duration::from_secs(30));
            // ponytail: one connection serialises every query (no SQLITE_BUSY between
            // writers); a reader pool next to one writer if it's ever too slow.
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
            sqlx::migrate!("./migrations/sqlite").run(&pool).await?;
            Pool::Lite(pool)
        } else {
            let pool = PgPoolOptions::new().max_connections(16).connect(url).await?;
            sqlx::migrate!("./migrations/postgres").run(&pool).await?;
            Pool::Pg(pool)
        };
        Ok(Self { pool, term: Default::default() })
    }

    pub fn is_sqlite(&self) -> bool {
        matches!(self.pool, Pool::Lite(_))
    }

    pub async fn agents(&self) -> Result<Vec<AgentRow>, sqlx::Error> {
        on!(self, |p, q| "select id, spec, epoch from agents order by created_at", "select id, spec, epoch from agents order by rowid" => {
            let rows = sqlx::query(q).fetch_all(p).await?;
            rows.into_iter()
                .map(|r| {
                    Ok(AgentRow {
                        id: r.try_get("id")?,
                        spec: r.try_get::<Json<Spec>, _>("spec")?.0,
                        epoch: r.try_get::<i64, _>("epoch")? as u64,
                    })
                })
                .collect()
        })
    }

    pub async fn create_agent(&self, id: AgentId, spec: &Spec) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into agents (id, parent, spec) values ($1, $2, $3)").bind(id).bind(spec.parent).bind(Json(spec)).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    pub async fn set_epoch(&self, id: AgentId, epoch: u64) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("update agents set epoch = $2 where id = $1").bind(id).bind(epoch as i64).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    /// Events after `after` (exclusive), in order.
    pub async fn events(&self, id: AgentId, after: u64) -> Result<Vec<Event>, sqlx::Error> {
        on!(self, |p| {
            let rows = sqlx::query("select event from events where agent_id = $1 and seq > $2 order by seq").bind(id).bind(after as i64).fetch_all(p).await?;
            rows.into_iter().map(|r| Ok(r.try_get::<Json<Event>, _>("event")?.0)).collect()
        })
    }

    /// Appends events starting at `first_seq`, atomically, if this hub's term
    /// is still the leader's. `Ok(false)` means a newer leader exists.
    pub async fn append(&self, id: AgentId, first_seq: u64, events: &[Event]) -> Result<bool, sqlx::Error> {
        // SQLite has one writer (one connection; the hub holds a lock file): no row lock.
        on!(self, |p, q| "select term from hub_leader where id = 1 for share", "select term from hub_leader where id = 1" => {
            let mut tx = p.begin().await?;
            let term: Option<i64> = sqlx::query_scalar(q).fetch_optional(&mut *tx).await?;
            if term != Some(self.term.load(std::sync::atomic::Ordering::SeqCst)) {
                return Ok(false);
            }
            for (i, e) in events.iter().enumerate() {
                sqlx::query("insert into events (agent_id, seq, event) values ($1, $2, $3)")
                    .bind(id)
                    .bind((first_seq + i as u64) as i64)
                    .bind(Json(e))
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    pub async fn put_mail(&self, to: &Addr, mail: &Mail) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into mail (addr, mail) values ($1, $2)").bind(to.to_string()).bind(Json(mail)).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    /// Takes all pending mail for `to`, oldest first.
    pub async fn take_mail(&self, to: &Addr) -> Result<Vec<Mail>, sqlx::Error> {
        on!(self, |p| {
            let rows = sqlx::query("update mail set taken = true where addr = $1 and not taken returning id, mail").bind(to.to_string()).fetch_all(p).await?;
            in_order(rows.into_iter().map(|r| Ok((r.try_get::<i64, _>("id")?, r.try_get::<Json<Mail>, _>("mail")?.0))))
        })
    }
}

/// Mail rows by id (`update … returning` gives them in no particular order).
fn in_order(rows: impl Iterator<Item = Result<(i64, Mail), sqlx::Error>>) -> Result<Vec<Mail>, sqlx::Error> {
    let mut v = rows.collect::<Result<Vec<_>, _>>()?;
    v.sort_by_key(|(id, _)| *id);
    Ok(v.into_iter().map(|(_, m)| m).collect())
}

/// A stored cluster file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ClusterFile {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct VersionInfo {
    pub version: i64,
    /// Unix seconds.
    pub applied_at: i64,
    pub applied_by: String,
}

/// A cluster version's columns, `at` in unix seconds, per backend.
const VERSION_PG: &str = "version, extract(epoch from applied_at)::bigint as at, applied_by";
const VERSION_LITE: &str = "version, applied_at as at, applied_by";

/// A row's `VersionInfo` (and its files when `files`), for either backend's row.
macro_rules! version_row {
    ($r:expr) => {
        VersionInfo { version: $r.try_get("version")?, applied_at: $r.try_get("at")?, applied_by: $r.try_get("applied_by")? }
    };
}

impl Db {
    pub async fn latest_cluster(&self) -> Result<Option<(VersionInfo, Vec<ClusterFile>)>, sqlx::Error> {
        on!(self, |p, cols| VERSION_PG, VERSION_LITE => {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("select {cols}, files from cluster_versions order by version desc limit 1"))).fetch_optional(p).await?;
            row.map(|r| Ok((version_row!(r), r.try_get::<Json<Vec<ClusterFile>>, _>("files")?.0))).transpose()
        })
    }

    pub async fn cluster_version(&self, v: i64) -> Result<Option<(VersionInfo, Vec<ClusterFile>)>, sqlx::Error> {
        on!(self, |p, cols| VERSION_PG, VERSION_LITE => {
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("select {cols}, files from cluster_versions where version = $1"))).bind(v).fetch_optional(p).await?;
            row.map(|r| Ok((version_row!(r), r.try_get::<Json<Vec<ClusterFile>>, _>("files")?.0))).transpose()
        })
    }

    pub async fn cluster_history(&self) -> Result<Vec<VersionInfo>, sqlx::Error> {
        on!(self, |p, cols| VERSION_PG, VERSION_LITE => {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!("select {cols} from cluster_versions order by version desc"))).fetch_all(p).await?;
            rows.iter().map(|r| Ok(version_row!(r))).collect()
        })
    }

    pub async fn add_cluster_version(&self, files: &[ClusterFile], by: &str) -> Result<VersionInfo, sqlx::Error> {
        on!(self, |p, cols| VERSION_PG, VERSION_LITE => {
            let r = sqlx::query(sqlx::AssertSqlSafe(format!("insert into cluster_versions (files, applied_by) values ($1, $2) returning {cols}")))
                .bind(Json(files))
                .bind(by)
                .fetch_one(p)
                .await?;
            Ok(version_row!(r))
        })
    }

    pub async fn tokens(&self) -> Result<Vec<(String, String, String)>, sqlx::Error> {
        on!(self, |p| {
            let rows = sqlx::query("select hash, kind, name from tokens").fetch_all(p).await?;
            rows.iter().map(|r| Ok((r.try_get("hash")?, r.try_get("kind")?, r.try_get("name")?))).collect()
        })
    }

    pub async fn add_token(&self, hash: &str, kind: &str, name: &str) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into tokens (hash, kind, name) values ($1, $2, $3)").bind(hash).bind(kind).bind(name).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    pub async fn revoke_tokens(&self, kind: &str, name: &str) -> Result<u64, sqlx::Error> {
        let affected = on!(self, |p| sqlx::query("delete from tokens where kind = $1 and name = $2").bind(kind).bind(name).execute(p).await?.rows_affected());
        Ok(affected)
    }
}

impl Db {
    pub async fn resident(&self, name: &str) -> Result<Option<uuid::Uuid>, sqlx::Error> {
        on!(self, |p| {
            let r = sqlx::query("select agent_id from residents where name = $1").bind(name).fetch_optional(p).await?;
            r.map(|r| r.try_get("agent_id")).transpose()
        })
    }

    pub async fn residents(&self) -> Result<Vec<(String, uuid::Uuid)>, sqlx::Error> {
        on!(self, |p| {
            let rows = sqlx::query("select name, agent_id from residents order by name").fetch_all(p).await?;
            rows.iter().map(|r| Ok((r.try_get("name")?, r.try_get("agent_id")?))).collect()
        })
    }

    pub async fn set_resident(&self, name: &str, id: uuid::Uuid) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into residents (name, agent_id) values ($1, $2) on conflict (name) do update set agent_id = $2").bind(name).bind(id).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    pub async fn remove_resident(&self, name: &str) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("delete from residents where name = $1").bind(name).execute(p).await.map(|_| ()))?;
        Ok(())
    }
}

impl Db {
    /// Takes up to `max` pending messages for `to`, oldest first.
    pub async fn take_mail_n(&self, to: &Addr, max: u32) -> Result<Vec<Mail>, sqlx::Error> {
        on!(self, |p, q| "update mail set taken = true
                          where id in (select id from mail where addr = $1 and not taken order by id limit $2 for update skip locked)
                          returning id, mail",
                         "update mail set taken = true
                          where id in (select id from mail where addr = $1 and not taken order by id limit $2)
                          returning id, mail" => {
            let rows = sqlx::query(q).bind(to.to_string()).bind(max as i64).fetch_all(p).await?;
            in_order(rows.into_iter().map(|r| Ok((r.try_get::<i64, _>("id")?, r.try_get::<Json<Mail>, _>("mail")?.0))))
        })
    }

    pub async fn peek_mail(&self, to: &Addr, max: u32) -> Result<Vec<Mail>, sqlx::Error> {
        on!(self, |p| {
            let rows = sqlx::query("select mail from mail where addr = $1 and not taken order by id limit $2").bind(to.to_string()).bind(max as i64).fetch_all(p).await?;
            rows.into_iter().map(|r| Ok(r.try_get::<Json<Mail>, _>("mail")?.0)).collect()
        })
    }
}

impl Db {
    pub async fn put_snapshot(&self, id: uuid::Uuid, seq: u64, state: &subnet_core::agent::Agent) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query(
            "insert into agent_snapshots (agent_id, seq, state) values ($1, $2, $3)
             on conflict (agent_id) do update set seq = $2, state = $3",
        )
        .bind(id)
        .bind(seq as i64)
        .bind(Json(state))
        .execute(p)
        .await.map(|_| ()))?;
        Ok(())
    }

    /// The newest snapshot taken before `seq` (exclusive).
    pub async fn snapshot_before(&self, id: uuid::Uuid, seq: u64) -> Result<Option<(u64, subnet_core::agent::Agent)>, sqlx::Error> {
        // SQLite's integers are signed 64-bit: u64::MAX would wrap.
        let seq = seq.min(i64::MAX as u64) as i64;
        on!(self, |p| {
            let r = sqlx::query("select seq, state from agent_snapshots where agent_id = $1 and seq < $2").bind(id).bind(seq).fetch_optional(p).await?;
            r.map(|r| Ok((r.try_get::<i64, _>("seq")? as u64, r.try_get::<Json<subnet_core::agent::Agent>, _>("state")?.0))).transpose()
        })
    }
}

/// A hold as kept: frozen or not, since when (unix ms), and what waits in
/// it (seq, route, payload), in order.
pub struct HoldRow {
    pub name: String,
    pub frozen: bool,
    pub since: Option<i64>,
    pub held: Vec<(i64, String, serde_json::Value)>,
}

/// A recorded switchboard delivery.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct DeliveryRow {
    pub id: i64,
    pub route: String,
    /// Unix milliseconds.
    pub at: i64,
    pub payload: serde_json::Value,
    /// One per action: `{"action": …, "ok": …}` or `{"action": …, "error": …}`.
    pub outcomes: serde_json::Value,
}

/// Now in unix milliseconds, in SQLite.
const LITE_NOW_MS: &str = "cast(unixepoch('subsec') * 1000 as integer)";

impl Db {
    /// Every hold with what it keeps.
    pub async fn holds(&self) -> Result<Vec<HoldRow>, sqlx::Error> {
        on!(self, |p, q| "select name, frozen, (extract(epoch from since) * 1000)::bigint as since from holds order by name",
                         "select name, frozen, since from holds order by name" => {
            let mut out: Vec<HoldRow> = sqlx::query(q)
                .fetch_all(p)
                .await?
                .iter()
                .map(|r| HoldRow { name: r.get("name"), frozen: r.get("frozen"), since: r.get("since"), held: vec![] })
                .collect();
            for h in &mut out {
                h.held = sqlx::query("select seq, route, payload from held where hold = $1 order by seq")
                    .bind(&h.name)
                    .fetch_all(p)
                    .await?
                    .iter()
                    .map(|r| (r.get("seq"), r.get("route"), r.get::<Json<serde_json::Value>, _>("payload").0))
                    .collect();
            }
            Ok(out)
        })
    }

    /// Freezes a hold, or releases it (and forgets what it kept).
    pub async fn set_hold(&self, name: &str, frozen: bool) -> Result<(), sqlx::Error> {
        let lite = format!(
            "insert into holds (name, frozen, since) values ($1, $2, case when $2 then {LITE_NOW_MS} end)
             on conflict (name) do update set frozen = $2, since = case when $2 then coalesce(holds.since, {LITE_NOW_MS}) end"
        );
        on!(self, |p, q| "insert into holds (name, frozen, since) values ($1, $2, case when $2 then now() end)
                          on conflict (name) do update set frozen = $2, since = case when $2 then coalesce(holds.since, now()) end".to_string(),
                         lite.clone() => {
            let mut tx = p.begin().await?;
            sqlx::query(sqlx::AssertSqlSafe(q)).bind(name).bind(frozen).execute(&mut *tx).await?;
            if !frozen {
                sqlx::query("delete from held where hold = $1").bind(name).execute(&mut *tx).await?;
            }
            tx.commit().await
        })
    }

    /// Keeps a delivery in a hold.
    pub async fn add_held(&self, hold: &str, seq: i64, route: &str, payload: &serde_json::Value) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into held (hold, seq, route, payload) values ($1, $2, $3, $4)").bind(hold).bind(seq).bind(route).bind(Json(payload)).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    /// Forgets a kept delivery (dropped: the hold was full).
    pub async fn drop_held(&self, hold: &str, seq: i64) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("delete from held where hold = $1 and seq = $2").bind(hold).bind(seq).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    pub async fn add_delivery(&self, route: &str, payload: &serde_json::Value, outcomes: &serde_json::Value) -> Result<(), sqlx::Error> {
        on!(self, |p| sqlx::query("insert into deliveries (route, payload, outcomes) values ($1, $2, $3)").bind(route).bind(Json(payload)).bind(Json(outcomes)).execute(p).await.map(|_| ()))?;
        Ok(())
    }

    pub async fn deliveries(&self, route: Option<&str>, limit: u32) -> Result<Vec<DeliveryRow>, sqlx::Error> {
        on!(self, |p, q| "select id, route, (extract(epoch from at) * 1000)::bigint as at, payload, outcomes from deliveries
                          where $1::text is null or route = $1 order by id desc limit $2",
                         "select id, route, at, payload, outcomes from deliveries
                          where $1 is null or route = $1 order by id desc limit $2" => {
            let rows = sqlx::query(q).bind(route).bind(limit as i64).fetch_all(p).await?;
            rows.iter()
                .map(|r| {
                    Ok(DeliveryRow {
                        id: r.try_get("id")?,
                        route: r.try_get("route")?,
                        at: r.try_get("at")?,
                        payload: r.try_get::<Json<serde_json::Value>, _>("payload")?.0,
                        outcomes: r.try_get::<Json<serde_json::Value>, _>("outcomes")?.0,
                    })
                })
                .collect()
        })
    }
}

impl Db {
    /// Stores a blob (idempotent: the same bytes are the same blob).
    pub async fn put_blob(&self, hash: &str, mime: &str, data: &[u8]) -> Result<(), sqlx::Error> {
        on!(self, |p, q| "insert into blobs (hash, mime, size, data) values ($1, $2, $3, $4) on conflict (hash) do update set touched_at = now()",
                         "insert into blobs (hash, mime, size, data) values ($1, $2, $3, $4) on conflict (hash) do update set touched_at = unixepoch()" => {
            sqlx::query(q).bind(hash).bind(mime).bind(data.len() as i64).bind(data).execute(p).await?;
            Ok(())
        })
    }

    /// A blob's type and bytes; reading it counts as a use.
    pub async fn get_blob(&self, hash: &str) -> Result<Option<(String, Vec<u8>)>, sqlx::Error> {
        on!(self, |p, q| "update blobs set touched_at = now() where hash = $1 returning mime, data",
                         "update blobs set touched_at = unixepoch() where hash = $1 returning mime, data" => {
            let r = sqlx::query(q).bind(hash).fetch_optional(p).await?;
            r.map(|r| Ok((r.try_get("mime")?, r.try_get("data")?))).transpose()
        })
    }

    /// Deletes blobs unused for `older_than_days`, except those an agent's
    /// log mentions (`blob:<sha256>`): its history stays reviewable,
    /// compacted parts included.
    pub async fn gc_blobs(&self, older_than_days: i64) -> Result<u64, sqlx::Error> {
        // ponytail: SQLite has no regexp; instr scans the events per old blob, fine for a daily sweep.
        let affected = on!(self, |p, q| "delete from blobs where touched_at < now() - make_interval(days => $1::int)
                          and hash not in (select (regexp_matches(event::text, 'blob:([0-9a-f]{64})', 'g'))[1] from events)",
                         "delete from blobs where touched_at < unixepoch() - $1 * 86400
                          and not exists (select 1 from events where instr(event, 'blob:' || blobs.hash) > 0)" => {
            sqlx::query(q).bind(older_than_days).execute(p).await?.rows_affected()
        });
        Ok(affected)
    }
}

impl Db {
    /// Takes the next leader term, advertising `url`; returns it.
    pub async fn take_term(&self, url: &str) -> Result<i64, sqlx::Error> {
        on!(self, |p, q| "insert into hub_leader (id, term, url) values (1, 1, $1)
                          on conflict (id) do update set term = hub_leader.term + 1, url = $1, since = now() returning term",
                         "insert into hub_leader (id, term, url) values (1, 1, $1)
                          on conflict (id) do update set term = hub_leader.term + 1, url = $1, since = unixepoch() returning term" => {
            sqlx::query_scalar(q).bind(url).fetch_one(p).await
        })
    }

    /// The leader's advertised URL, as recorded.
    pub async fn leader_url(&self) -> Result<Option<String>, sqlx::Error> {
        on!(self, |p| sqlx::query_scalar("select url from hub_leader where id = 1").fetch_optional(p).await)
    }

    /// The SQLite file behind the pool (none: in memory, or Postgres).
    pub fn sqlite_file(&self) -> Option<std::path::PathBuf> {
        match &self.pool {
            Pool::Lite(p) => Some(p.connect_options().get_filename().to_path_buf()).filter(|f| !f.as_os_str().is_empty() && f.as_os_str() != ":memory:"),
            Pool::Pg(_) => None,
        }
    }
}
