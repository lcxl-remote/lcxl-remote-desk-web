//! Injected configuration follows a connection into every runtime transaction.

use super::{ConfigConnection, ConfigContext, ConfigRead};
use async_trait::async_trait;
use sea_orm::{
    AccessMode, ConnectionTrait, DbBackend, DbErr, ExecResult, IsolationLevel, QueryResult,
    Statement, TransactionError, TransactionSession, TransactionTrait,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug)]
pub struct DatabaseConnection {
    inner: sea_orm::DatabaseConnection,
    configuration: Arc<ConfigContext>,
}

#[derive(Debug)]
pub struct DatabaseTransaction {
    inner: sea_orm::DatabaseTransaction,
    configuration: Arc<ConfigContext>,
    snapshot: ConfigRead,
    hold: HoldTimer,
}

/// The production pool has one connection, so a long transaction delays every other
/// database user. Name the holder and split its time between SQLite statements and
/// named phases to make that queueing attributable.
#[derive(Debug)]
struct HoldTimer {
    began: std::time::Instant,
    caller: Option<&'static std::panic::Location<'static>>,
    statements: AtomicU64,
    statement_micros: AtomicU64,
    phases: Mutex<Vec<(&'static str, u64)>>,
}

const NOTABLE_HOLD: std::time::Duration = std::time::Duration::from_millis(200);
const SLOW_HOLD: std::time::Duration = std::time::Duration::from_secs(1);

impl HoldTimer {
    fn new() -> Self {
        Self {
            began: std::time::Instant::now(),
            caller: None,
            statements: AtomicU64::new(0),
            statement_micros: AtomicU64::new(0),
            phases: Mutex::new(Vec::new()),
        }
    }

    async fn statement<T>(&self, run: impl Future<Output = T>) -> T {
        let started = std::time::Instant::now();
        let result = run.await;
        self.statements.fetch_add(1, Ordering::Relaxed);
        self.statement_micros.fetch_add(
            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        result
    }

    fn mark(&self, phase: &'static str) {
        let at = u64::try_from(self.began.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Ok(mut phases) = self.phases.lock() {
            phases.push((phase, at));
        }
    }

    /// Phase values are milliseconds since the transaction began. Time outside
    /// statements is CPU work or waiting for the async runtime to resume this task.
    fn summary(&self) -> String {
        let phases = self
            .phases
            .lock()
            .map(|phases| {
                phases
                    .iter()
                    .map(|(name, at)| format!("{name}@{at}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        format!(
            "statements={} statement_ms={} phases=[{phases}]",
            self.statements.load(Ordering::Relaxed),
            self.statement_micros.load(Ordering::Relaxed) / 1000,
        )
    }
}

impl Drop for HoldTimer {
    fn drop(&mut self) {
        let held = self.began.elapsed();
        if held < NOTABLE_HOLD {
            return;
        }
        let level = if held >= SLOW_HOLD {
            log::Level::Warn
        } else {
            log::Level::Info
        };
        let summary = self.summary();
        match self.caller {
            Some(caller) => log::log!(
                level,
                "[db] write transaction from {caller} held the connection for {} ms: {summary}",
                held.as_millis()
            ),
            None => log::log!(
                level,
                "[db] transaction held the connection for {} ms: {summary}",
                held.as_millis()
            ),
        }
    }
}

impl DatabaseConnection {
    pub fn new(inner: sea_orm::DatabaseConnection, configuration: Arc<ConfigContext>) -> Self {
        Self {
            inner,
            configuration,
        }
    }

    pub async fn close(self) -> Result<(), DbErr> {
        self.inner.close().await
    }
}

impl std::ops::Deref for DatabaseConnection {
    type Target = sea_orm::DatabaseConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[async_trait]
impl ConfigConnection for DatabaseConnection {
    fn config_context(&self) -> &Arc<ConfigContext> {
        &self.configuration
    }
    async fn config_read(&self) -> ConfigRead {
        self.configuration.read().await
    }
}
#[async_trait]
impl ConfigConnection for DatabaseTransaction {
    fn config_context(&self) -> &Arc<ConfigContext> {
        &self.configuration
    }
    async fn config_read(&self) -> ConfigRead {
        self.snapshot.clone()
    }
}

macro_rules! connection {
    ($kind:ty) => {
        #[async_trait]
        impl ConnectionTrait for $kind {
            fn get_database_backend(&self) -> DbBackend {
                self.inner.get_database_backend()
            }
            async fn execute_raw(&self, statement: Statement) -> Result<ExecResult, DbErr> {
                self.inner.execute_raw(statement).await
            }
            async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
                self.inner.execute_unprepared(sql).await
            }
            async fn query_one_raw(
                &self,
                statement: Statement,
            ) -> Result<Option<QueryResult>, DbErr> {
                self.inner.query_one_raw(statement).await
            }
            async fn query_all_raw(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
                self.inner.query_all_raw(statement).await
            }
            fn is_mock_connection(&self) -> bool {
                self.inner.is_mock_connection()
            }
        }
    };
}
connection!(DatabaseConnection);

#[async_trait]
impl ConnectionTrait for DatabaseTransaction {
    fn get_database_backend(&self) -> DbBackend {
        self.inner.get_database_backend()
    }
    async fn execute_raw(&self, statement: Statement) -> Result<ExecResult, DbErr> {
        self.hold.statement(self.inner.execute_raw(statement)).await
    }
    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, DbErr> {
        self.hold
            .statement(self.inner.execute_unprepared(sql))
            .await
    }
    async fn query_one_raw(&self, statement: Statement) -> Result<Option<QueryResult>, DbErr> {
        self.hold
            .statement(self.inner.query_one_raw(statement))
            .await
    }
    async fn query_all_raw(&self, statement: Statement) -> Result<Vec<QueryResult>, DbErr> {
        self.hold
            .statement(self.inner.query_all_raw(statement))
            .await
    }
    fn is_mock_connection(&self) -> bool {
        self.inner.is_mock_connection()
    }
}

macro_rules! transactions {
    ($kind:ty, $snapshot:expr) => {
        #[async_trait]
        impl TransactionTrait for $kind {
            type Transaction = DatabaseTransaction;
            async fn begin(&self) -> Result<Self::Transaction, DbErr> {
                self.begin_with_config(None, None).await
            }
            async fn begin_with_config(
                &self,
                isolation: Option<IsolationLevel>,
                access: Option<AccessMode>,
            ) -> Result<Self::Transaction, DbErr> {
                let snapshot = ($snapshot)(self).await;
                let inner = self.inner.begin_with_config(isolation, access).await?;
                Ok(DatabaseTransaction {
                    inner,
                    configuration: self.configuration.clone(),
                    snapshot,
                    hold: HoldTimer::new(),
                })
            }
            async fn transaction<F, T, E>(&self, callback: F) -> Result<T, TransactionError<E>>
            where
                F: for<'c> FnOnce(
                        &'c Self::Transaction,
                    )
                        -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'c>>
                    + Send,
                T: Send,
                E: std::fmt::Display + std::fmt::Debug + Send,
            {
                self.transaction_with_config(callback, None, None).await
            }
            async fn transaction_with_config<F, T, E>(
                &self,
                callback: F,
                isolation: Option<IsolationLevel>,
                access: Option<AccessMode>,
            ) -> Result<T, TransactionError<E>>
            where
                F: for<'c> FnOnce(
                        &'c Self::Transaction,
                    )
                        -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'c>>
                    + Send,
                T: Send,
                E: std::fmt::Display + std::fmt::Debug + Send,
            {
                let txn = self
                    .begin_with_config(isolation, access)
                    .await
                    .map_err(TransactionError::Connection)?;
                match callback(&txn).await {
                    Ok(value) => {
                        txn.commit().await.map_err(TransactionError::Connection)?;
                        Ok(value)
                    }
                    Err(error) => {
                        txn.rollback().await.map_err(TransactionError::Connection)?;
                        Err(TransactionError::Transaction(error))
                    }
                }
            }
        }
    };
}
transactions!(DatabaseConnection, ConfigConnection::config_read);
transactions!(DatabaseTransaction, ConfigConnection::config_read);

#[async_trait]
impl TransactionSession for DatabaseTransaction {
    async fn commit(self) -> Result<(), DbErr> {
        let Self { inner, hold, .. } = self;
        hold.mark("before_commit");
        let result = inner.commit().await;
        hold.mark("committed");
        result
    }
    async fn rollback(self) -> Result<(), DbErr> {
        self.inner.rollback().await
    }
}
impl DatabaseTransaction {
    pub(crate) fn name_holder(&mut self, caller: &'static std::panic::Location<'static>) {
        self.hold.caller = Some(caller);
    }
    /// Records a named point in the slow-transaction log line.
    pub(crate) fn mark_phase(&self, phase: &'static str) {
        self.hold.mark(phase);
    }
    pub async fn commit(self) -> Result<(), DbErr> {
        TransactionSession::commit(self).await
    }
    pub async fn rollback(self) -> Result<(), DbErr> {
        TransactionSession::rollback(self).await
    }
}

#[cfg(test)]
impl DatabaseTransaction {
    pub(crate) fn holder(&self) -> Option<&'static std::panic::Location<'static>> {
        self.hold.caller
    }
    pub(crate) fn hold_summary(&self) -> String {
        self.hold.summary()
    }
}
