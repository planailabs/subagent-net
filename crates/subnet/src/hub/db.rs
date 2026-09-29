//! Postgres persistence for the hub. Schema changes go through `migrations/`.

use sqlx::{PgPool, Row, postgres::PgPoolOptions, types::Json};
use subnet_core::addr::{Addr, AgentId};
use subnet_core::agent::{Event, Spec};
use subnet_core::proto::Mail;

pub struct Db(pub PgPool);

pub struct AgentRow {
    pub id: AgentId,
    pub spec: Spec,
    pub epoch: u64,
}

impl Db {
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new().max_connections(16).connect(url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self(pool))
    }

    pub async fn agents(&self) -> Result<Vec<AgentRow>, sqlx::Error> {
        let rows = sqlx::query("select id, spec, epoch from agents order by created_at").fetch_all(&self.0).await?;
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
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn set_epoch(&self, id: AgentId, epoch: u64) -> Result<(), sqlx::Error> {
        sqlx::query("update agents set epoch = $2 where id = $1").bind(id).bind(epoch as i64).execute(&self.0).await?;
        Ok(())
    }

    /// Events after `after` (exclusive), in order.
    pub async fn events(&self, id: AgentId, after: u64) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query("select event from events where agent_id = $1 and seq > $2 order by seq")
            .bind(id)
            .bind(after as i64)
            .fetch_all(&self.0)
            .await?;
        rows.into_iter().map(|r| Ok(r.try_get::<Json<Event>, _>("event")?.0)).collect()
    }

    /// Appends events starting at `first_seq`; atomically.
    pub async fn append(&self, id: AgentId, first_seq: u64, events: &[Event]) -> Result<(), sqlx::Error> {
        let mut tx = self.0.begin().await?;
        for (i, e) in events.iter().enumerate() {
            sqlx::query("insert into events (agent_id, seq, event) values ($1, $2, $3)")
                .bind(id)
                .bind((first_seq + i as u64) as i64)
                .bind(Json(e))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }

    pub async fn put_mail(&self, to: &Addr, mail: &Mail) -> Result<(), sqlx::Error> {
        sqlx::query("insert into mail (addr, mail) values ($1, $2)")
            .bind(to.to_string())
            .bind(Json(mail))
            .execute(&self.0)
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
        .fetch_all(&self.0)
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
        .fetch_optional(&self.0)
        .await?;
        row.map(|r| Self::version_row(&r)).transpose()
    }

    pub async fn cluster_version(&self, v: i64) -> Result<Option<(VersionInfo, Vec<ClusterFile>)>, sqlx::Error> {
        let row = sqlx::query(
            "select version, extract(epoch from applied_at)::bigint as at, applied_by, files
             from cluster_versions where version = $1",
        )
        .bind(v)
        .fetch_optional(&self.0)
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
        .fetch_all(&self.0)
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
        .fetch_one(&self.0)
        .await?;
        Ok(VersionInfo { version: r.try_get("version")?, applied_at: r.try_get("at")?, applied_by: r.try_get("applied_by")? })
    }

    pub async fn tokens(&self) -> Result<Vec<(String, String, String)>, sqlx::Error> {
        let rows = sqlx::query("select hash, kind, name from tokens").fetch_all(&self.0).await?;
        rows.iter().map(|r| Ok((r.try_get("hash")?, r.try_get("kind")?, r.try_get("name")?))).collect()
    }

    pub async fn add_token(&self, hash: &str, kind: &str, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("insert into tokens (hash, kind, name) values ($1, $2, $3)")
            .bind(hash)
            .bind(kind)
            .bind(name)
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn revoke_tokens(&self, kind: &str, name: &str) -> Result<u64, sqlx::Error> {
        let r = sqlx::query("delete from tokens where kind = $1 and name = $2").bind(kind).bind(name).execute(&self.0).await?;
        Ok(r.rows_affected())
    }
}

impl Db {
    pub async fn resident(&self, name: &str) -> Result<Option<uuid::Uuid>, sqlx::Error> {
        let r = sqlx::query("select agent_id from residents where name = $1").bind(name).fetch_optional(&self.0).await?;
        r.map(|r| r.try_get("agent_id")).transpose()
    }

    pub async fn residents(&self) -> Result<Vec<(String, uuid::Uuid)>, sqlx::Error> {
        let rows = sqlx::query("select name, agent_id from residents order by name").fetch_all(&self.0).await?;
        rows.iter().map(|r| Ok((r.try_get("name")?, r.try_get("agent_id")?))).collect()
    }

    pub async fn set_resident(&self, name: &str, id: uuid::Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("insert into residents (name, agent_id) values ($1, $2) on conflict (name) do update set agent_id = $2")
            .bind(name)
            .bind(id)
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn remove_resident(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("delete from residents where name = $1").bind(name).execute(&self.0).await?;
        Ok(())
    }
}
