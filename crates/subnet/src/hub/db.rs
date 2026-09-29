//! Postgres persistence for the hub. Schema changes go through `migrations/`.

use sqlx::{PgPool, Row, postgres::PgPoolOptions, types::Json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Event, Spec};
use subnet_core::proto::Mail;

pub struct Db {
    pub pool: PgPool,
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
        let pool = PgPoolOptions::new().max_connections(16).connect(url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool, term: Default::default() })
    }

    pub async fn agents(&self) -> Result<Vec<AgentRow>, sqlx::Error> {
        let rows = sqlx::query("select id, spec, epoch from agents order by created_at").fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| {
                Ok(AgentRow {
                    id: r.try_get("id")?,
                    spec: r.try_get::<Json<Spec>, _>("spec")?.0,
                    epoch: r.try_get::<i64, _>("epoch")? as u64,
                })
            })
            .collect()
    }

    pub async fn create_agent(&self, id: AgentId, spec: &Spec) -> Result<(), sqlx::Error> {
        sqlx::query("insert into agents (id, parent, spec) values ($1, $2, $3)")
            .bind(id)
            .bind(spec.parent)
            .bind(Json(spec))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_epoch(&self, id: AgentId, epoch: u64) -> Result<(), sqlx::Error> {
        sqlx::query("update agents set epoch = $2 where id = $1").bind(id).bind(epoch as i64).execute(&self.pool).await?;
        Ok(())
    }

    /// Events after `after` (exclusive), in order.
    pub async fn events(&self, id: AgentId, after: u64) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query("select event from events where agent_id = $1 and seq > $2 order by seq")
            .bind(id)
            .bind(after as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(|r| Ok(r.try_get::<Json<Event>, _>("event")?.0)).collect()
    }

    /// Appends events starting at `first_seq`, atomically, if this hub's term
    /// is still the leader's. `Ok(false)` means a newer leader exists.
    pub async fn append(&self, id: AgentId, first_seq: u64, events: &[Event]) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let term: Option<i64> =
            sqlx::query_scalar("select term from hub_leader where id = 1 for share").fetch_optional(&mut *tx).await?;
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
    }

    pub async fn put_mail(&self, to: &Addr, mail: &Mail) -> Result<(), sqlx::Error> {
        sqlx::query("insert into mail (addr, mail) values ($1, $2)")
            .bind(to.to_string())
            .bind(Json(mail))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Takes all pending mail for `to`, oldest first.
    pub async fn take_mail(&self, to: &Addr) -> Result<Vec<Mail>, sqlx::Error> {
        let rows = sqlx::query(
            "with t as (update mail set taken = true where addr = $1 and not taken returning id, mail)
             select mail from t order by id",
        )
        .bind(to.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| Ok(r.try_get::<Json<Mail>, _>("mail")?.0)).collect()
    }
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

impl Db {
    pub async fn latest_cluster(&self) -> Result<Option<(VersionInfo, Vec<ClusterFile>)>, sqlx::Error> {
        let row = sqlx::query(
            "select version, extract(epoch from applied_at)::bigint as at, applied_by, files
             from cluster_versions order by version desc limit 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Self::version_row(&r)).transpose()
    }

    pub async fn cluster_version(&self, v: i64) -> Result<Option<(VersionInfo, Vec<ClusterFile>)>, sqlx::Error> {
        let row = sqlx::query(
            "select version, extract(epoch from applied_at)::bigint as at, applied_by, files
             from cluster_versions where version = $1",
        )
        .bind(v)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Self::version_row(&r)).transpose()
    }

    fn version_row(r: &sqlx::postgres::PgRow) -> Result<(VersionInfo, Vec<ClusterFile>), sqlx::Error> {
        Ok((
            VersionInfo { version: r.try_get("version")?, applied_at: r.try_get("at")?, applied_by: r.try_get("applied_by")? },
            r.try_get::<Json<Vec<ClusterFile>>, _>("files")?.0,
        ))
    }

    pub async fn cluster_history(&self) -> Result<Vec<VersionInfo>, sqlx::Error> {
        let rows = sqlx::query(
            "select version, extract(epoch from applied_at)::bigint as at, applied_by
             from cluster_versions order by version desc",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(VersionInfo { version: r.try_get("version")?, applied_at: r.try_get("at")?, applied_by: r.try_get("applied_by")? })
            })
            .collect()
    }

    pub async fn add_cluster_version(&self, files: &[ClusterFile], by: &str) -> Result<VersionInfo, sqlx::Error> {
        let r = sqlx::query(
            "insert into cluster_versions (files, applied_by) values ($1, $2)
             returning version, extract(epoch from applied_at)::bigint as at, applied_by",
        )
        .bind(Json(files))
        .bind(by)
        .fetch_one(&self.pool)
        .await?;
        Ok(VersionInfo { version: r.try_get("version")?, applied_at: r.try_get("at")?, applied_by: r.try_get("applied_by")? })
    }

    pub async fn tokens(&self) -> Result<Vec<(String, String, String)>, sqlx::Error> {
        let rows = sqlx::query("select hash, kind, name from tokens").fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok((r.try_get("hash")?, r.try_get("kind")?, r.try_get("name")?))).collect()
    }

    pub async fn add_token(&self, hash: &str, kind: &str, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("insert into tokens (hash, kind, name) values ($1, $2, $3)")
            .bind(hash)
            .bind(kind)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn revoke_tokens(&self, kind: &str, name: &str) -> Result<u64, sqlx::Error> {
        let r = sqlx::query("delete from tokens where kind = $1 and name = $2").bind(kind).bind(name).execute(&self.pool).await?;
        Ok(r.rows_affected())
    }
}

impl Db {
    pub async fn resident(&self, name: &str) -> Result<Option<uuid::Uuid>, sqlx::Error> {
        let r = sqlx::query("select agent_id from residents where name = $1").bind(name).fetch_optional(&self.pool).await?;
        r.map(|r| r.try_get("agent_id")).transpose()
    }

    pub async fn residents(&self) -> Result<Vec<(String, uuid::Uuid)>, sqlx::Error> {
        let rows = sqlx::query("select name, agent_id from residents order by name").fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok((r.try_get("name")?, r.try_get("agent_id")?))).collect()
    }

    pub async fn set_resident(&self, name: &str, id: uuid::Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("insert into residents (name, agent_id) values ($1, $2) on conflict (name) do update set agent_id = $2")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_resident(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("delete from residents where name = $1").bind(name).execute(&self.pool).await?;
        Ok(())
    }
}

impl Db {
    /// Takes up to `max` pending messages for `to`, oldest first.
    pub async fn take_mail_n(&self, to: &Addr, max: u32) -> Result<Vec<Mail>, sqlx::Error> {
        let rows = sqlx::query(
            "with t as (
                 update mail set taken = true
                 where id in (select id from mail where addr = $1 and not taken order by id limit $2 for update skip locked)
                 returning id, mail)
             select mail from t order by id",
        )
        .bind(to.to_string())
        .bind(max as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| Ok(r.try_get::<Json<Mail>, _>("mail")?.0)).collect()
    }

    pub async fn peek_mail(&self, to: &Addr, max: u32) -> Result<Vec<Mail>, sqlx::Error> {
        let rows = sqlx::query("select mail from mail where addr = $1 and not taken order by id limit $2")
            .bind(to.to_string())
            .bind(max as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(|r| Ok(r.try_get::<Json<Mail>, _>("mail")?.0)).collect()
    }
}

impl Db {
    pub async fn put_snapshot(&self, id: uuid::Uuid, seq: u64, state: &subnet_core::agent::Agent) -> Result<(), sqlx::Error> {
        sqlx::query(
            "insert into agent_snapshots (agent_id, seq, state) values ($1, $2, $3)
             on conflict (agent_id) do update set seq = $2, state = $3",
        )
        .bind(id)
        .bind(seq as i64)
        .bind(Json(state))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The newest snapshot taken before `seq` (exclusive).
    pub async fn snapshot_before(&self, id: uuid::Uuid, seq: u64) -> Result<Option<(u64, subnet_core::agent::Agent)>, sqlx::Error> {
        let r = sqlx::query("select seq, state from agent_snapshots where agent_id = $1 and seq < $2")
            .bind(id)
            .bind(seq as i64)
            .fetch_optional(&self.pool)
            .await?;
        r.map(|r| Ok((r.try_get::<i64, _>("seq")? as u64, r.try_get::<Json<subnet_core::agent::Agent>, _>("state")?.0)))
            .transpose()
    }
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

impl Db {
    pub async fn add_delivery(&self, route: &str, payload: &serde_json::Value, outcomes: &serde_json::Value) -> Result<(), sqlx::Error> {
        sqlx::query("insert into deliveries (route, payload, outcomes) values ($1, $2, $3)")
            .bind(route)
            .bind(Json(payload))
            .bind(Json(outcomes))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn deliveries(&self, route: Option<&str>, limit: u32) -> Result<Vec<DeliveryRow>, sqlx::Error> {
        let rows = sqlx::query(
            "select id, route, (extract(epoch from at) * 1000)::bigint as at, payload, outcomes from deliveries
             where $1::text is null or route = $1 order by id desc limit $2",
        )
        .bind(route)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
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
    }
}

impl Db {
    /// Stores a blob (idempotent: the same bytes are the same blob).
    pub async fn put_blob(&self, hash: &str, mime: &str, data: &[u8]) -> Result<(), sqlx::Error> {
        sqlx::query(
            "insert into blobs (hash, mime, size, data) values ($1, $2, $3, $4)
             on conflict (hash) do update set touched_at = now()",
        )
        .bind(hash)
        .bind(mime)
        .bind(data.len() as i64)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A blob's type and bytes; reading it counts as a use.
    pub async fn get_blob(&self, hash: &str) -> Result<Option<(String, Vec<u8>)>, sqlx::Error> {
        let r = sqlx::query("update blobs set touched_at = now() where hash = $1 returning mime, data")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        r.map(|r| Ok((r.try_get("mime")?, r.try_get("data")?))).transpose()
    }

    pub async fn gc_blobs(&self, older_than_days: i64) -> Result<u64, sqlx::Error> {
        let r = sqlx::query("delete from blobs where touched_at < now() - make_interval(days => $1::int)")
            .bind(older_than_days)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
