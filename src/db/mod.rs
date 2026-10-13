//! Database connectivity — the "GORM" driver layer of this framework.
//!
//! A [`Database`] is a sea-orm connection pool. [`connect`] builds one from the `db`
//! section of the configuration file, so the driver is a deployment detail:
//!
//! ```yaml
//! db:
//!   type: sqlite          # sqlite | mysql | postgres
//!   path: ./data.db       # sqlite only
//! ```
//!
//! ```yaml
//! db:
//!   type: mysql
//!   host: localhost
//!   port: 3306
//!   user: root
//!   password: secret
//!   database: mydb
//! ```
//!
//! Anything sea-orm can reach is reachable with a raw `url`, so ClickHouse, SQL Server
//! and friends need no framework support at all.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{
    ConnectOptions, ConnectionTrait, DatabaseConnection, DbBackend, ExecResult, QueryResult,
    Statement, TransactionError, TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::web::{Error, Result, code};

pub use sea_orm;

/// A handle to a database connection.
///
/// Most of the time this is the connection *pool*, and cloning it is cheap. Inside
/// [`transaction`] it is the transaction instead, which is why the model layer takes a
/// `Database` rather than a `DatabaseConnection`: the same model code then runs on the
/// pool or inside a transaction without changing shape.
#[derive(Clone)]
pub struct Database(Arc<Handle>);

enum Handle {
    Pool(DatabaseConnection),
    Transaction(sea_orm::DatabaseTransaction),
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("backend", &self.backend())
            .field("inTransaction", &self.is_transaction())
            .finish()
    }
}

impl Database {
    /// Wraps a connection pool.
    #[must_use]
    pub fn pool(connection: DatabaseConnection) -> Self {
        Self(Arc::new(Handle::Pool(connection)))
    }

    /// Wraps an open transaction.
    #[must_use]
    pub fn transaction_handle(transaction: sea_orm::DatabaseTransaction) -> Self {
        Self(Arc::new(Handle::Transaction(transaction)))
    }

    /// The underlying pool, when this handle is not a transaction.
    #[must_use]
    pub fn pool_connection(&self) -> Option<&DatabaseConnection> {
        match &*self.0 {
            Handle::Pool(connection) => Some(connection),
            Handle::Transaction(_) => None,
        }
    }

    /// The SQL dialect behind this handle.
    #[must_use]
    pub fn backend(&self) -> DbBackend {
        match &*self.0 {
            Handle::Pool(connection) => connection.get_database_backend(),
            Handle::Transaction(transaction) => transaction.get_database_backend(),
        }
    }

    /// Reports whether this handle is a transaction rather than the pool.
    #[must_use]
    pub fn is_transaction(&self) -> bool {
        matches!(&*self.0, Handle::Transaction(_))
    }

    /// Runs `body` in a transaction, committing when it returns `Ok` and rolling back
    /// when it returns `Err`.
    ///
    /// Inside `body`, every model built from the handle it is given — or from any clone of
    /// it — participates in the transaction. Calling this on a handle that is already a
    /// transaction opens a savepoint instead.
    ///
    /// ```no_run
    /// # use rust_web_frame::db::Database;
    /// # use rust_web_frame::web::Result;
    /// # async fn demo(db: Database) -> Result<()> {
    /// db.transaction(|tx| async move {
    ///     // every query against `tx` — or any clone of it — runs inside the transaction
    ///     let _ = tx.backend();
    ///     Ok(())
    /// })
    /// .await
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when the transaction cannot be started or committed. A handle that leaked
    /// out of `body` cannot be committed; the transaction rolls back and an error says so.
    pub async fn transaction<F, Fut, T>(&self, body: F) -> Result<T>
    where
        F: FnOnce(Database) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        let transaction = match &*self.0 {
            Handle::Pool(connection) => TransactionTrait::begin(connection).await,
            // A transaction beginning another transaction opens a savepoint.
            Handle::Transaction(transaction) => TransactionTrait::begin(transaction).await,
        }
        .map_err(Error::from)?;

        let scoped = Database::transaction_handle(transaction);
        match body(scoped.clone()).await {
            Ok(value) => {
                scoped.commit().await?;
                Ok(value)
            }
            Err(error) => {
                scoped.rollback().await;
                Err(error)
            }
        }
    }

    /// Commits the transaction. Consumes the handle.
    async fn commit(self) -> Result<()> {
        match Arc::try_unwrap(self.0) {
            Ok(Handle::Transaction(transaction)) => transaction.commit().await.map_err(Error::from),
            Ok(Handle::Pool(_)) => Ok(()),
            Err(_) => Err(Error::internal().detail(
                "the transaction handle outlived its scope and was rolled back instead of committed",
            )),
        }
    }

    /// Rolls the transaction back. Consumes the handle.
    async fn rollback(self) {
        if let Ok(Handle::Transaction(transaction)) = Arc::try_unwrap(self.0) {
            let _ = transaction.rollback().await;
        }
    }

    /// Reports whether the connection is alive.
    ///
    /// # Errors
    ///
    /// Fails when the database cannot be reached.
    pub async fn ping(&self) -> Result<()> {
        self.execute_unprepared("SELECT 1")
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    /// Closes the pool. Does nothing for a transaction handle.
    ///
    /// # Errors
    ///
    /// Fails when the pool cannot be closed cleanly.
    pub async fn close(self) -> Result<()> {
        match Arc::try_unwrap(self.0) {
            Ok(Handle::Pool(connection)) => connection.close().await.map_err(Error::from),
            Ok(Handle::Transaction(transaction)) => {
                let _ = transaction.rollback().await;
                Ok(())
            }
            Err(_) => Err(Error::internal()
                .detail("the database handle is still in use elsewhere and cannot be closed")),
        }
    }
}

#[async_trait]
impl ConnectionTrait for Database {
    fn get_database_backend(&self) -> DbBackend {
        self.backend()
    }

    async fn execute_raw(&self, statement: Statement) -> Result<ExecResult, sea_orm::DbErr> {
        match &*self.0 {
            Handle::Pool(connection) => connection.execute_raw(statement).await,
            Handle::Transaction(transaction) => transaction.execute_raw(statement).await,
        }
    }

    async fn execute_unprepared(&self, sql: &str) -> Result<ExecResult, sea_orm::DbErr> {
        match &*self.0 {
            Handle::Pool(connection) => connection.execute_unprepared(sql).await,
            Handle::Transaction(transaction) => transaction.execute_unprepared(sql).await,
        }
    }

    async fn query_one_raw(
        &self,
        statement: Statement,
    ) -> Result<Option<QueryResult>, sea_orm::DbErr> {
        match &*self.0 {
            Handle::Pool(connection) => connection.query_one_raw(statement).await,
            Handle::Transaction(transaction) => transaction.query_one_raw(statement).await,
        }
    }

    async fn query_all_raw(
        &self,
        statement: Statement,
    ) -> Result<Vec<QueryResult>, sea_orm::DbErr> {
        match &*self.0 {
            Handle::Pool(connection) => connection.query_all_raw(statement).await,
            Handle::Transaction(transaction) => transaction.query_all_raw(statement).await,
        }
    }
}

/// Config section that holds the database settings.
pub const DB_CONFIG_KEY: &str = "db";

/// Database configuration, read from the `db` section of the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DbConfig {
    /// Driver name: `sqlite`, `mysql` or `postgres`.
    #[serde(rename = "type")]
    pub driver: String,
    /// A complete connection string. When set, it overrides every field below.
    pub url: Option<String>,
    /// SQLite only: the database file. `:memory:` for an in-memory database.
    pub path: Option<String>,
    /// Server host.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Login name.
    pub user: String,
    /// Password.
    pub password: String,
    /// Schema / database name.
    pub database: String,
    /// Maximum number of pooled connections.
    pub max_connections: u32,
    /// Minimum number of connections kept warm.
    pub min_connections: u32,
    /// Seconds to wait for a connection before giving up.
    pub connect_timeout: u64,
    /// Seconds an idle connection is kept before being closed.
    pub idle_timeout: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            driver: String::from("sqlite"),
            url: None,
            path: Some(String::from("./data.db")),
            host: String::from("localhost"),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            max_connections: 20,
            min_connections: 1,
            connect_timeout: 30,
            idle_timeout: 600,
        }
    }
}

impl DbConfig {
    /// A SQLite configuration backed by `path`.
    pub fn sqlite(path: impl Into<String>) -> Self {
        Self {
            driver: String::from("sqlite"),
            path: Some(path.into()),
            ..Self::default()
        }
    }

    /// An in-memory SQLite database, useful in tests.
    pub fn sqlite_memory() -> Self {
        Self::sqlite(":memory:")
    }

    /// A MySQL configuration.
    pub fn mysql(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        password: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self {
            driver: String::from("mysql"),
            url: None,
            host: host.into(),
            port,
            user: user.into(),
            password: password.into(),
            database: database.into(),
            ..Self::default()
        }
    }

    /// A PostgreSQL configuration.
    pub fn postgres(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        password: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self {
            driver: String::from("postgres"),
            url: None,
            host: host.into(),
            port,
            user: user.into(),
            password: password.into(),
            database: database.into(),
            ..Self::default()
        }
    }

    /// The connection string this configuration resolves to.
    pub fn connection_url(&self) -> Result<String> {
        if let Some(url) = &self.url
            && !url.is_empty()
        {
            return Ok(url.clone());
        }
        match self.driver.to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" => {
                let path = self
                    .path
                    .clone()
                    .unwrap_or_else(|| String::from(":memory:"));
                if path == ":memory:" || path.is_empty() {
                    Ok(String::from("sqlite::memory:"))
                } else {
                    // `mode=rwc` creates the file when it does not exist yet.
                    Ok(format!("sqlite://{path}?mode=rwc"))
                }
            }
            "mysql" => {
                let port = if self.port == 0 { 3306 } else { self.port };
                Ok(format!(
                    "mysql://{}:{}@{}:{}/{}",
                    self.user, self.password, self.host, port, self.database
                ))
            }
            "postgres" | "postgresql" => {
                let port = if self.port == 0 { 5432 } else { self.port };
                Ok(format!(
                    "postgres://{}:{}@{}:{}/{}",
                    self.user, self.password, self.host, port, self.database
                ))
            }
            other => Err(Error::new(
                code::INTERNAL_ERROR,
                format!("unsupported database type `{other}`"),
            )
            .detail(
                "supported types are sqlite, mysql and postgres — set `db.url` for anything else",
            )),
        }
    }
}

/// Connects using the `db` section of `config`. Returns `None` when the section is absent.
pub async fn connect_from_config(config: &Config) -> Result<Option<Database>> {
    if !config.contains_key(DB_CONFIG_KEY) {
        return Ok(None);
    }
    let db_config: DbConfig = config.unmarshal_key(DB_CONFIG_KEY)?;
    Ok(Some(connect(&db_config).await?))
}

/// Opens a connection pool.
///
/// ```no_run
/// # use rust_web_frame::db::{connect, DbConfig};
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let db = connect(&DbConfig::sqlite("./data.db")).await?;
/// # Ok(())
/// # }
/// ```
pub async fn connect(config: &DbConfig) -> Result<Database> {
    let url = config.connection_url()?;
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(config.max_connections.max(1))
        .min_connections(config.min_connections)
        .connect_timeout(Duration::from_secs(if config.connect_timeout == 0 {
            30
        } else {
            config.connect_timeout
        }))
        .sqlx_logging(false);

    if config.idle_timeout > 0 {
        options.idle_timeout(Duration::from_secs(config.idle_timeout));
    }

    let pool = sea_orm::Database::connect(options).await.map_err(|err| {
        Error::with_source(code::SERVICE_UNAVAILABLE, err)
            .detail(format!("cannot connect to the {} database", config.driver))
    })?;

    tracing::info!(driver = %config.driver, "database connected");
    Ok(Database::pool(pool))
}

/// Opens an in-memory SQLite database. Handy in tests.
pub async fn connect_memory() -> Result<Database> {
    connect(&DbConfig::sqlite_memory()).await
}

/// Runs `body` inside a transaction, committing when it returns `Ok` and rolling back
/// when it returns `Err`.
///
/// ```no_run
/// # use rust_web_frame::db::{Database, transaction};
/// # use rust_web_frame::web::{Error, Result};
/// # async fn demo(db: Database) -> Result<()> {
/// transaction(&db, |tx| async move {
///     // every query against `tx` participates in the transaction
///     drop(tx);
///     Ok(())
/// })
/// .await
/// # }
/// ```
pub async fn transaction<F, Fut, T>(db: &Database, body: F) -> Result<T>
where
    F: FnOnce(Database) -> Fut + Send,
    Fut: std::future::Future<Output = Result<T>> + Send,
    T: Send,
{
    db.transaction(body).await
}

/// Converts a sea-orm transaction error into the framework's error type.
pub fn transaction_error(error: TransactionError<Error>) -> Error {
    Error::from(error)
}

/// Reports whether `table` exists.
pub async fn has_table(db: &Database, table: &str) -> Result<bool> {
    use sea_orm::ConnectionTrait;

    let backend = db.get_database_backend();
    let statement = match backend {
        DbBackend::Sqlite => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            [table.into()],
        ),
        DbBackend::MySql => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?",
            [table.into()],
        ),
        DbBackend::Postgres => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT tablename FROM pg_tables WHERE schemaname = current_schema() AND tablename = ?",
            [table.into()],
        ),
        // `DatabaseBackend` is `#[non_exhaustive]`; an unknown driver is treated as
        // "cannot tell", which makes `create_table` fall through to `CREATE TABLE IF NOT
        // EXISTS` and `Model::is_exist` return `false`.
        _ => return Ok(false),
    };

    let rows = db.query_all_raw(statement).await?;
    Ok(!rows.is_empty())
}

/// Drops `table` when it exists.
pub async fn drop_table_if_exists(db: &Database, table: &str) -> Result<()> {
    use sea_orm::ConnectionTrait;
    db.execute_unprepared(&format!("DROP TABLE IF EXISTS \"{table}\""))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_urls_ask_for_create_if_missing() {
        let config = DbConfig::sqlite("./data.db");
        assert_eq!(
            config.connection_url().unwrap(),
            "sqlite://./data.db?mode=rwc"
        );

        let memory = DbConfig::sqlite_memory();
        assert_eq!(memory.connection_url().unwrap(), "sqlite::memory:");
    }

    #[test]
    fn server_urls_fill_in_the_default_port() {
        let mysql = DbConfig::mysql("db.internal", 0, "root", "pw", "shop");
        assert_eq!(
            mysql.connection_url().unwrap(),
            "mysql://root:pw@db.internal:3306/shop"
        );

        let postgres = DbConfig::postgres("db.internal", 0, "postgres", "pw", "shop");
        assert_eq!(
            postgres.connection_url().unwrap(),
            "postgres://postgres:pw@db.internal:5432/shop"
        );
    }

    #[test]
    fn an_explicit_url_always_wins() {
        let mut config = DbConfig::sqlite("./ignored.db");
        config.url = Some(String::from("clickhouse://localhost:9000/default"));
        assert_eq!(
            config.connection_url().unwrap(),
            "clickhouse://localhost:9000/default"
        );
    }

    #[test]
    fn unknown_drivers_are_rejected_with_a_useful_message() {
        let config = DbConfig {
            driver: String::from("oracle"),
            ..DbConfig::default()
        };
        let err = config.connection_url().unwrap_err();
        assert!(err.message.contains("unsupported database type"));
        assert!(err.client_message().contains("sqlite, mysql and postgres"));
    }

    #[tokio::test]
    async fn an_in_memory_database_connects_and_answers_queries() {
        let db = connect_memory().await.unwrap();
        assert!(!has_table(&db, "t_absent").await.unwrap());
    }
}
