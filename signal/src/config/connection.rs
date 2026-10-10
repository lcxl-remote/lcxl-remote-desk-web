//! Injected configuration follows a connection into every runtime transaction.

use super::{ConfigConnection, ConfigContext, ConfigRead};
use async_trait::async_trait;
use sea_orm::{
    AccessMode, ConnectionTrait, DbBackend, DbErr, ExecResult, IsolationLevel, QueryResult,
    Statement, TransactionError, TransactionSession, TransactionTrait,
};
use std::{future::Future, pin::Pin, sync::Arc};

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
connection!(DatabaseTransaction);

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
        self.inner.commit().await
    }
    async fn rollback(self) -> Result<(), DbErr> {
        self.inner.rollback().await
    }
}
impl DatabaseTransaction {
    pub async fn commit(self) -> Result<(), DbErr> {
        TransactionSession::commit(self).await
    }
    pub async fn rollback(self) -> Result<(), DbErr> {
        TransactionSession::rollback(self).await
    }
}
