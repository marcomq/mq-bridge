//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `load_mode`: one transaction for a whole drain, committed by the disconnect hook when
//! the route completes and rolled back otherwise. Readers see the old rows until then.

use super::CopyPool;
use crate::models::SqlLoadMode;
use crate::traits::{DisconnectOutcome, PublisherError};
use anyhow::anyhow;
use sqlx::AnyPool;
use tracing::{info, warn};

/// The transaction a load writes into: on the COPY pool when `bulk_copy` is on.
pub(super) enum HeldTx {
    Any(sqlx::Transaction<'static, sqlx::Any>),
    #[cfg(feature = "sqlx")]
    Pg(sqlx::Transaction<'static, sqlx::Postgres>),
}

#[cfg(feature = "sqlx")]
pub(super) type CopyConn = sqlx::PgConnection;
/// Without the PostgreSQL driver there is no COPY connection.
#[cfg(not(feature = "sqlx"))]
pub(super) enum CopyConn {}

impl HeldTx {
    pub(super) fn any(&mut self) -> Result<&mut sqlx::AnyConnection, PublisherError> {
        match self {
            Self::Any(tx) => Ok(&mut **tx),
            #[cfg(feature = "sqlx")]
            Self::Pg(_) => Err(mismatch()),
        }
    }

    pub(super) fn pg(&mut self) -> Result<&mut CopyConn, PublisherError> {
        match self {
            #[cfg(feature = "sqlx")]
            Self::Pg(tx) => Ok(&mut **tx),
            Self::Any(_) => Err(mismatch()),
        }
    }

    async fn execute(&mut self, sql: &str) -> Result<(), sqlx::Error> {
        let sql = super::audited_sql(sql);
        match self {
            Self::Any(tx) => sqlx::query(sql).execute(&mut **tx).await.map(drop),
            #[cfg(feature = "sqlx")]
            Self::Pg(tx) => sqlx::query(sql).execute(&mut **tx).await.map(drop),
        }
    }

    async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            Self::Any(tx) => tx.commit().await,
            #[cfg(feature = "sqlx")]
            Self::Pg(tx) => tx.commit().await,
        }
    }

    async fn rollback(self) -> Result<(), sqlx::Error> {
        match self {
            Self::Any(tx) => tx.rollback().await,
            #[cfg(feature = "sqlx")]
            Self::Pg(tx) => tx.rollback().await,
        }
    }
}

fn mismatch() -> PublisherError {
    PublisherError::NonRetryable(anyhow!(
        "load_mode: the held transaction is on the wrong connection type"
    ))
}

#[derive(Default)]
pub(super) struct LoadState {
    tx: Option<HeldTx>,
    /// Why the load can no longer commit; every later write fails with it.
    poisoned: Option<String>,
    finished: bool,
}

pub(super) struct HeldLoad {
    mode: SqlLoadMode,
    table: String,
    state: tokio::sync::Mutex<LoadState>,
}

impl HeldLoad {
    pub(super) fn new(mode: SqlLoadMode, table: &str) -> Self {
        Self {
            mode,
            table: table.to_string(),
            state: Default::default(),
        }
    }

    /// Locks the held transaction for one write, opening it (and, for `replace`, deleting the
    /// old rows) on the first one. Until [`LoadWrite::settle`] the load counts as failed.
    pub(super) async fn lock(
        &self,
        pool: &AnyPool,
        copy_pool: Option<&CopyPool>,
    ) -> Result<LoadWrite<'_>, PublisherError> {
        let mut state = self.state.lock().await;
        if state.finished {
            return Err(PublisherError::Connection(anyhow!(
                "load_mode: the load was already committed or rolled back"
            )));
        }
        if let Some(reason) = &state.poisoned {
            return Err(PublisherError::Connection(anyhow!(
                "load_mode: an earlier write of this load failed, it will be rolled back: {reason}"
            )));
        }
        state.poisoned = Some("a write was interrupted".into());
        let mut write = LoadWrite { state };
        if write.state.tx.is_none() {
            let opened = async {
                let mut tx = begin(pool, copy_pool).await?;
                if self.mode == SqlLoadMode::Replace {
                    tx.execute(&format!("DELETE FROM {}", self.table))
                        .await
                        .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
                }
                Ok(tx)
            }
            .await;
            match opened {
                Ok(tx) => write.state.tx = Some(tx),
                Err(e) => return Err(write.fail(e)),
            }
        }
        Ok(write)
    }

    /// Commits a completed load; rolls back anything else. A poisoned load fails a completed route.
    pub(super) async fn finish(&self) -> anyhow::Result<()> {
        let outcome = crate::traits::disconnect_outcome().unwrap_or(DisconnectOutcome::Stopped);
        let mut state = self.state.lock().await;
        state.finished = true;
        let tx = state.tx.take();
        let table = &self.table;
        match (outcome, state.poisoned.take(), tx) {
            (DisconnectOutcome::Completed, None, Some(tx)) => {
                tx.commit().await.map_err(|e| {
                    anyhow!(e).context(format!(
                        "load_mode: committing the load into '{table}' failed; nothing was written"
                    ))
                })?;
                info!(table = %table, "load_mode: load committed");
                Ok(())
            }
            (DisconnectOutcome::Completed, None, None) => {
                match self.mode {
                    SqlLoadMode::Replace => warn!(
                        table = %table,
                        "load_mode replace: the source was empty, the table is left unchanged"
                    ),
                    SqlLoadMode::AtomicAppend => {
                        info!(table = %table, "load_mode: the source was empty, nothing to commit")
                    }
                }
                Ok(())
            }
            (outcome, poisoned, tx) => {
                if let Some(tx) = tx {
                    if let Err(e) = tx.rollback().await {
                        warn!(table = %table, error = %e, "load_mode: rollback failed; the database discards the open transaction itself");
                    }
                }
                warn!(table = %table, ?outcome, "load_mode: load rolled back, the table is unchanged");
                match (outcome, poisoned) {
                    (DisconnectOutcome::Completed, Some(reason)) => Err(anyhow!(
                        "load_mode: a write into '{table}' failed, the load was rolled back: {reason}"
                    )),
                    _ => Ok(()),
                }
            }
        }
    }
}

impl Drop for HeldLoad {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        if !state.finished && state.tx.is_some() {
            warn!(table = %self.table, "load_mode: load dropped before it finished; the database rolls it back, the table is unchanged");
        }
    }
}

/// One write into the held transaction; holds the load's lock.
pub(super) struct LoadWrite<'a> {
    state: tokio::sync::MutexGuard<'a, LoadState>,
}

impl LoadWrite<'_> {
    pub(super) fn tx(&mut self) -> Result<&mut HeldTx, PublisherError> {
        self.state.tx.as_mut().ok_or_else(mismatch)
    }

    /// Any failure poisons the load: from now on it can only roll back. Reported as a
    /// connection error, so the route stops instead of dropping (or dead-lettering) every later batch.
    pub(super) fn settle<T>(
        mut self,
        result: Result<T, PublisherError>,
    ) -> Result<T, PublisherError> {
        match result {
            Ok(value) => {
                self.state.poisoned = None;
                Ok(value)
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn fail(&mut self, e: PublisherError) -> PublisherError {
        let e = match e {
            PublisherError::Retryable(e)
            | PublisherError::NonRetryable(e)
            | PublisherError::Connection(e) => e,
        };
        self.state.poisoned = Some(format!("{e:#}"));
        PublisherError::Connection(
            e.context("load_mode: write failed, the load will be rolled back"),
        )
    }
}

async fn begin(pool: &AnyPool, copy_pool: Option<&CopyPool>) -> Result<HeldTx, PublisherError> {
    let failed = |e: sqlx::Error| PublisherError::NonRetryable(anyhow!(e));
    match copy_pool {
        #[cfg(feature = "sqlx")]
        Some(copy_pool) => Ok(HeldTx::Pg(copy_pool.begin().await.map_err(failed)?)),
        #[cfg(not(feature = "sqlx"))]
        Some(copy_pool) => match *copy_pool {},
        None => Ok(HeldTx::Any(pool.begin().await.map_err(failed)?)),
    }
}
