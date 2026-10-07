//! High availability: hubs sharing one database elect a leader through a
//! Postgres advisory lock (with SQLite: an exclusive lock on a file next to
//! the database). Standbys refuse work and point at the leader.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::{Connection, PgConnection};

use super::{Hub, HubError, State};

/// Advisory lock key for "the leader of this database".
const LOCK_KEY: i64 = 0x5375_626e_6574; // "Subnet"
const TRY_EVERY: Duration = Duration::from_millis(200);
const CHECK_EVERY: Duration = Duration::from_secs(1);

impl Hub {
    pub fn is_leader(&self) -> bool {
        self.leader.load(Ordering::SeqCst)
    }

    /// The leader's advertised URL, as last seen.
    pub fn leader_url(&self) -> Option<String> {
        self.leader_url.read().unwrap().clone()
    }

    /// Waits until this hub leads.
    pub async fn wait_leader(&self) {
        loop {
            let n = self.leader_changed.notified();
            if self.is_leader() {
                return;
            }
            n.await;
        }
    }

    /// Stops background work and gives up leadership (the lock is released).
    pub fn shutdown(&self) {
        self.stop.cancel();
    }

    /// Competes for leadership for as long as the hub runs.
    pub(crate) async fn elect(self: Arc<Self>, db_url: String, advertise: String) {
        if self.db.is_sqlite() {
            return self.elect_sqlite(advertise).await;
        }
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            let mut conn = match PgConnection::connect(&db_url).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "election: cannot reach the database");
                    self.idle(TRY_EVERY * 5).await;
                    continue;
                }
            };
            // Standby until the lock is ours.
            loop {
                if self.stop.is_cancelled() {
                    return;
                }
                match sqlx::query_scalar::<_, bool>("select pg_try_advisory_lock($1)").bind(LOCK_KEY).fetch_one(&mut conn).await {
                    Ok(true) => break,
                    Ok(false) => {
                        let url: Option<String> =
                            sqlx::query_scalar("select url from hub_leader where id = 1").fetch_optional(&mut conn).await.ok().flatten();
                        *self.leader_url.write().unwrap() = url;
                        self.idle(TRY_EVERY).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "election: lost the database");
                        break;
                    }
                }
            }
            if conn.ping().await.is_err() {
                continue;
            }
            let term: Result<i64, _> = sqlx::query_scalar(
                "insert into hub_leader (id, term, url) values (1, 1, $1)
                 on conflict (id) do update set term = hub_leader.term + 1, url = $1, since = now()
                 returning term",
            )
            .bind(&advertise)
            .fetch_one(&mut conn)
            .await;
            let Ok(term) = term else { continue };
            self.db.term.store(term, Ordering::SeqCst);
            *self.leader_url.write().unwrap() = Some(advertise.clone());
            if let Err(e) = self.load().await {
                tracing::error!(error = %e, "election: loading state failed");
                self.step_down().await;
                continue;
            }
            self.fenced.store(false, Ordering::SeqCst);
            self.leader.store(true, Ordering::SeqCst);
            self.leader_changed.notify_waiters();
            tracing::info!(term, url = %advertise, "leading");
            // Hold the lock while the connection lives and no newer leader fenced us.
            loop {
                tokio::select! {
                    _ = self.stop.cancelled() => break,
                    _ = tokio::time::sleep(CHECK_EVERY) => {}
                }
                if self.fenced.load(Ordering::SeqCst) {
                    tracing::error!("fenced by a newer leader");
                    break;
                }
                if let Err(e) = conn.ping().await {
                    tracing::error!(error = %e, "lost the leader lock connection");
                    break;
                }
            }
            self.step_down().await;
            drop(conn); // releases the advisory lock
        }
    }

    /// SQLite: whoever holds an exclusive lock on `<db>.hub-lock` leads (a
    /// database in memory belongs to this process alone). The lock goes with
    /// the process, so a crashed leader's standby takes over.
    async fn elect_sqlite(self: Arc<Self>, advertise: String) {
        let lock = match self.db.sqlite_file() {
            Some(f) => {
                let mut name = f.into_os_string();
                name.push(".hub-lock");
                match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&name) {
                    Ok(file) => Some(file),
                    Err(e) => {
                        tracing::error!(error = %e, path = ?name, "election: cannot open the lock file");
                        return;
                    }
                }
            }
            None => None,
        };
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            // Standby until the lock is ours.
            match lock.as_ref().map(|f| f.try_lock()) {
                None | Some(Ok(())) => {}
                Some(Err(std::fs::TryLockError::WouldBlock)) => {
                    *self.leader_url.write().unwrap() = self.db.leader_url().await.ok().flatten();
                    self.idle(TRY_EVERY).await;
                    continue;
                }
                Some(Err(std::fs::TryLockError::Error(e))) => {
                    tracing::error!(error = %e, "election: cannot lock");
                    self.idle(TRY_EVERY * 5).await;
                    continue;
                }
            }
            let term = match self.db.take_term(&advertise).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "election: cannot take a term");
                    if let Some(f) = &lock {
                        let _ = f.unlock();
                    }
                    self.idle(TRY_EVERY * 5).await;
                    continue;
                }
            };
            self.db.term.store(term, Ordering::SeqCst);
            *self.leader_url.write().unwrap() = Some(advertise.clone());
            if let Err(e) = self.load().await {
                tracing::error!(error = %e, "election: loading state failed");
                self.step_down().await;
                if let Some(f) = &lock {
                    let _ = f.unlock();
                }
                self.idle(TRY_EVERY * 5).await;
                continue;
            }
            self.fenced.store(false, Ordering::SeqCst);
            self.leader.store(true, Ordering::SeqCst);
            self.leader_changed.notify_waiters();
            tracing::info!(term, url = %advertise, "leading (sqlite)");
            loop {
                tokio::select! {
                    _ = self.stop.cancelled() => break,
                    _ = tokio::time::sleep(CHECK_EVERY) => {}
                }
                if self.fenced.load(Ordering::SeqCst) {
                    tracing::error!("fenced by a newer leader");
                    break;
                }
            }
            self.step_down().await;
            if let Some(f) = &lock {
                let _ = f.unlock();
            }
        }
    }

    async fn idle(&self, d: Duration) {
        tokio::select! {
            _ = self.stop.cancelled() => {}
            _ = tokio::time::sleep(d) => {}
        }
    }

    /// Stops leading: all in-memory state goes (nodes see their connection
    /// close and reconnect to the new leader).
    pub(crate) async fn step_down(&self) {
        if self.leader.swap(false, Ordering::SeqCst) {
            tracing::warn!("stepping down");
        }
        self.db.term.store(0, Ordering::SeqCst);
        // Don't send clients back here; the next standby loop learns the new leader.
        *self.leader_url.write().unwrap() = None;
        *self.st.lock().await = State::default();
        self.leader_changed.notify_waiters();
    }

    /// Called when a write was fenced off: a newer leader exists.
    pub(crate) fn fence(&self) -> HubError {
        self.fenced.store(true, Ordering::SeqCst);
        self.leader.store(false, Ordering::SeqCst);
        HubError::NotLeader
    }
}
