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
