//! Database connectivity — the "GORM" driver layer of this framework.
//!
//! A [`Database`] is a sea-orm connection pool. [`connect`] builds one from the `db`
//! section of the configuration file, which *is* sea-orm's [`ConnectOptions`]: a
//! connection string, and one key per setter.
//!
//! ```yaml
//! db:
//!   url: postgres://user:secret@localhost:5432/shop
//!   maxConnections: 20
//!   acquireTimeout: 5
//! ```
//!
//! The url is sqlx's, so the scheme picks the driver and there is nothing to keep in
//! step with a separate `type` field:
//!
//! ```yaml
//! db:
//!   url: sqlite://./data.db?mode=rwc    # `mode=rwc` creates the file when it is missing
//! ```
//!
//! Anything sea-orm can reach is reachable this way — ClickHouse, SQL Server and friends
//! need no framework support at all.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{
    ConnectOptions, ConnectionTrait, DatabaseConnection, DbBackend, ExecResult, QueryResult,
    Statement, TransactionTrait,
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
    pub(crate) fn transaction_handle(transaction: sea_orm::DatabaseTransaction) -> Self {
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
    /// **Use the handle the closure is given, and only that one.** Two mistakes both end in
    /// a hang rather than an error, because both leave a connection checked out with no one
    /// to return it:
    ///
    /// * querying through the *pool* while the transaction is open — the pool hands out a
    ///   second connection, which for SQLite is a second, empty database, and for a
    ///   saturated pool simply never arrives;
    /// * letting a clone of the handle escape the closure — into a spawned task, or into a
    ///   struct that outlives it — after which the transaction can neither be committed nor
    ///   dropped.
    ///
    /// `acquireTimeout` bounds the wait for a pool connection, so the first mistake ends in
    /// an error after that long instead of never. The second has no bound: the commit
    /// reports that the handle escaped, and the connection stays out of the pool until
    /// every clone of it is dropped.
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
///
/// `url` is the connection string sea-orm takes and everything else is one of
/// [`ConnectOptions`]' settings, under the setter's own name — the config is the struct,
/// rather than a translation of it. Each setting is `Option`, and `None` means "leave
/// sea-orm's default alone", so nothing here invents a default that differs from the
/// library's. The closure-valued hooks (`map_sqlite_connect_options`, `after_connect`)
/// have no equivalent; build a `ConnectOptions` by hand for those.
///
/// ```yaml
/// db:
///   url: postgres://user:secret@localhost:5432/shop
///   maxConnections: 20
///   minConnections: 2
///   acquireTimeout: 5          # seconds to wait for a free connection
///   maxLifetime: 1800          # seconds before a connection is recycled
///   statementTimeout: 30       # seconds a single statement may run
///   slowStatementThreshold: 1
///   sqlLogging: true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DbConfig {
    /// The connection string, exactly as [`ConnectOptions::new`] takes it: `sqlite:…`,
    /// `mysql://…`, `postgres://…`.
    pub url: String,
    /// [`ConnectOptions::max_connections`].
    pub max_connections: Option<u32>,
    /// [`ConnectOptions::min_connections`].
    pub min_connections: Option<u32>,
    /// [`ConnectOptions::connect_timeout`], in seconds.
    pub connect_timeout: Option<u64>,
    /// [`ConnectOptions::acquire_timeout`], in seconds — how long a query waits for a free
    /// connection. The one that turns pool starvation into an error instead of a hang.
    pub acquire_timeout: Option<u64>,
    /// [`ConnectOptions::idle_timeout`], in seconds. `0` closes idle connections at once.
    pub idle_timeout: Option<u64>,
    /// [`ConnectOptions::max_lifetime`], in seconds.
    pub max_lifetime: Option<u64>,
    /// [`ConnectOptions::statement_timeout`], in seconds.
    pub statement_timeout: Option<u64>,
    /// [`ConnectOptions::test_before_acquire`].
    pub test_before_acquire: Option<bool>,
    /// [`ConnectOptions::test_before_acquire_if_idle_for`], in seconds.
    pub test_before_acquire_if_idle_for: Option<u64>,
    /// [`ConnectOptions::connect_lazy`] — build the pool without connecting, so the
    /// application starts even when the database is briefly unavailable.
    pub connect_lazy: Option<bool>,
    /// [`ConnectOptions::sqlx_logging`].
    pub sql_logging: Option<bool>,
    /// [`ConnectOptions::sqlx_logging_level`] as a `tracing` filter directive:
    /// `trace`, `debug`, `info`, `warn`, `error`, `off`.
    pub sql_logging_level: Option<String>,
    /// [`ConnectOptions::sqlx_slow_statements_logging_settings`] threshold, in seconds.
    /// Setting it implies a level of `warn`.
    pub slow_statement_threshold: Option<u64>,
    /// [`ConnectOptions::record_stmt_in_spans`].
    pub record_stmt_in_spans: Option<bool>,
}

impl DbConfig {
    /// Applies every configured option to `options`, and reports the ones it set.
    ///
    /// A field that is `None` is left alone, so sea-orm's default stands.
    fn apply(&self, options: &mut ConnectOptions) -> Vec<&'static str> {
        let mut applied = Vec::new();

        if let Some(value) = self.max_connections {
            options.max_connections(value);
            applied.push("maxConnections");
        }
        if let Some(value) = self.min_connections {
            options.min_connections(value);
            applied.push("minConnections");
        }
        if let Some(seconds) = self.connect_timeout {
            options.connect_timeout(Duration::from_secs(seconds));
            applied.push("connectTimeout");
        }
        if let Some(seconds) = self.acquire_timeout {
            options.acquire_timeout(Duration::from_secs(seconds));
            applied.push("acquireTimeout");
        }
        if let Some(seconds) = self.idle_timeout {
            options.idle_timeout(Duration::from_secs(seconds));
            applied.push("idleTimeout");
        }
        if let Some(seconds) = self.max_lifetime {
            options.max_lifetime(Duration::from_secs(seconds));
            applied.push("maxLifetime");
        }
        if let Some(seconds) = self.statement_timeout {
            options.statement_timeout(Duration::from_secs(seconds));
            applied.push("statementTimeout");
        }
        if let Some(value) = self.test_before_acquire {
            options.test_before_acquire(value);
            applied.push("testBeforeAcquire");
        }
        if let Some(seconds) = self.test_before_acquire_if_idle_for {
            options.test_before_acquire_if_idle_for(Duration::from_secs(seconds));
            applied.push("testBeforeAcquireIfIdleFor");
        }
        if let Some(value) = self.connect_lazy {
            options.connect_lazy(value);
            applied.push("connectLazy");
        }
        if let Some(value) = self.sql_logging {
            options.sqlx_logging(value);
            applied.push("sqlLogging");
        }
        if let Some(level) = self.sql_logging_level.as_deref().and_then(parse_level) {
            options.sqlx_logging_level(level);
            applied.push("sqlLoggingLevel");
        }
        if let Some(seconds) = self.slow_statement_threshold {
            options.sqlx_slow_statements_logging_settings(
                log::LevelFilter::Warn,
                Duration::from_secs(seconds),
            );
            applied.push("slowStatementThreshold");
        }
        if let Some(value) = self.record_stmt_in_spans {
            options.record_stmt_in_spans(value);
            applied.push("recordStmtInSpans");
        }

        applied
    }

    /// The options that matter enough to state even when they were not configured.
    ///
    /// A pool with no acquire bound waits for a free connection for as long as the holder
    /// takes — which, for a transaction whose handle leaked, is forever.
    fn defaults_worth_setting(&self, options: &mut ConnectOptions) {
        if self.acquire_timeout.is_none() {
            options.acquire_timeout(Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECONDS));
        }
    }
}

/// How long a query waits for a free pooled connection when nothing says otherwise.
pub const DEFAULT_ACQUIRE_TIMEOUT_SECONDS: u64 = 30;

/// Parses a `tracing` level name into the `log` level sqlx expects.
fn parse_level(name: &str) -> Option<log::LevelFilter> {
    match name.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(log::LevelFilter::Trace),
        "debug" => Some(log::LevelFilter::Debug),
        "info" => Some(log::LevelFilter::Info),
        "warn" | "warning" => Some(log::LevelFilter::Warn),
        "error" => Some(log::LevelFilter::Error),
        "off" => Some(log::LevelFilter::Off),
        _ => None,
    }
}

impl Default for DbConfig {
    /// An empty configuration: no url, and every option left to sea-orm.
    ///
    /// There is deliberately no default database — a `db` section that names no url is a
    /// mistake, and [`connect`] says so rather than quietly opening `./data.db`.
    fn default() -> Self {
        Self {
            url: String::new(),
            max_connections: None,
            min_connections: None,
            connect_timeout: None,
            acquire_timeout: None,
            idle_timeout: None,
            max_lifetime: None,
            statement_timeout: None,
            test_before_acquire: None,
            test_before_acquire_if_idle_for: None,
            connect_lazy: None,
            sql_logging: None,
            sql_logging_level: None,
            slow_statement_threshold: None,
            record_stmt_in_spans: None,
        }
    }
}

impl DbConfig {
    /// A configuration for `url`.
    ///
    /// ```no_run
    /// # use rust_web_frame::db::DbConfig;
    /// let config = DbConfig::new("postgres://user:secret@localhost:5432/shop");
    /// ```
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    /// A SQLite configuration backed by `path`, creating the file when it is missing.
    ///
    /// ```no_run
    /// # use rust_web_frame::db::DbConfig;
    /// let config = DbConfig::sqlite("./data.db");
    /// assert_eq!(config.url, "sqlite://./data.db?mode=rwc");
    /// ```
    pub fn sqlite(path: impl Into<String>) -> Self {
        let path = path.into();
        if path.is_empty() || path == ":memory:" {
            return Self::sqlite_memory();
        }
        // sqlx opens an existing file and fails otherwise; `mode=rwc` is what asks it to
        // create one. Only this constructor spells it out — a url from a config file is
        // passed to sea-orm untouched.
        Self::new(format!("sqlite://{path}?mode=rwc"))
    }

    /// An in-memory SQLite database, useful in tests.
    pub fn sqlite_memory() -> Self {
        Self::new("sqlite::memory:")
    }
}

/// Connects using the `db` section of `config`. Returns `None` when the section is absent.
pub async fn connect_from_config(config: &Config) -> Result<Option<Database>> {
    if !config.contains_key(DB_CONFIG_KEY) {
        return Ok(None);
    }
    let db_config: DbConfig = config.section(DB_CONFIG_KEY)?;
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
    let url = config.url.trim();
    if url.is_empty() {
        return Err(Error::new(
            code::INTERNAL_ERROR,
            "the `db` section has no connection url",
        )
        .detail(
            "set `db.url` to a connection string — `sqlite://./data.db?mode=rwc`, \
             `mysql://user:secret@localhost:3306/shop`, `postgres://user:secret@localhost/shop`",
        ));
    }

    let mut options = ConnectOptions::new(url);
    config.defaults_worth_setting(&mut options);
    let applied = config.apply(&mut options);

    let pool = sea_orm::Database::connect(options).await.map_err(|err| {
        Error::with_source(code::SERVICE_UNAVAILABLE, err)
            .detail(format!("cannot connect to {}", redacted_url(url)))
    })?;

    tracing::info!(
        database = %redacted_url(url),
        options = ?applied,
        "database connected"
    );
    Ok(Database::pool(pool))
}

/// `url` with any password in it replaced, for logs and error messages.
///
/// A connection string carries credentials more often than not, and neither a log file nor
/// a stack trace is the place for them.
fn redacted_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // `sqlite::memory:` and `sqlite:./data.db` have no authority to hold credentials.
        return url.to_owned();
    };
    // A password may contain an `@`, so the last one delimits the authority.
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return url.to_owned();
    };
    let Some((user, _password)) = userinfo.split_once(':') else {
        return url.to_owned();
    };
    format!("{scheme}://{user}:***@{host}")
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
        // EXISTS` and `Model::exists` return `false`.
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
    fn the_sqlite_helper_asks_for_create_if_missing() {
        assert_eq!(
            DbConfig::sqlite("./data.db").url,
            "sqlite://./data.db?mode=rwc"
        );
        assert_eq!(DbConfig::sqlite(":memory:").url, "sqlite::memory:");
        assert_eq!(DbConfig::sqlite("").url, "sqlite::memory:");
    }

    #[test]
    fn the_db_section_is_sea_orms_connect_options_one_key_each() {
        let config: DbConfig = serde_json::from_value(serde_json::json!({
            "url": "postgres://root:pw@db.internal:5432/shop",
            "maxConnections": 20,
            "minConnections": 2,
            "acquireTimeout": 5,
            "maxLifetime": 1800,
            "statementTimeout": 30,
            "slowStatementThreshold": 1,
            "sqlLogging": true,
            "sqlLoggingLevel": "debug",
            "connectLazy": true
        }))
        .unwrap();

        assert_eq!(config.url, "postgres://root:pw@db.internal:5432/shop");
        assert_eq!(config.max_connections, Some(20));
        assert_eq!(config.acquire_timeout, Some(5));
        assert_eq!(config.max_lifetime, Some(1800));
        assert_eq!(config.statement_timeout, Some(30));
        assert_eq!(config.slow_statement_threshold, Some(1));
        assert_eq!(config.sql_logging_level.as_deref(), Some("debug"));
        assert_eq!(config.connect_lazy, Some(true));

        // Every configured key reaches sea-orm.
        let mut options = ConnectOptions::new(&config.url);
        let applied = config.apply(&mut options);
        for expected in [
            "maxConnections",
            "minConnections",
            "acquireTimeout",
            "maxLifetime",
            "statementTimeout",
            "slowStatementThreshold",
            "sqlLogging",
            "sqlLoggingLevel",
            "connectLazy",
        ] {
            assert!(
                applied.contains(&expected),
                "{expected} was not applied: {applied:?}"
            );
        }
    }

    #[test]
    fn an_unconfigured_database_still_gets_an_acquire_bound() {
        // Without one, a query waits for a free connection for as long as the holder takes.
        let mut options = ConnectOptions::new("postgres://localhost/x");
        DbConfig::default().defaults_worth_setting(&mut options);
        assert_eq!(
            options.get_acquire_timeout(),
            Some(Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECONDS))
        );

        // And an explicit value is respected rather than overridden.
        let mut options = ConnectOptions::new("postgres://localhost/x");
        DbConfig {
            acquire_timeout: Some(3),
            ..DbConfig::default()
        }
        .defaults_worth_setting(&mut options);
        assert_eq!(
            options.get_acquire_timeout(),
            None,
            "the default does not overwrite"
        );
    }

    #[test]
    fn log_levels_parse_the_way_a_config_file_writes_them() {
        assert_eq!(parse_level("debug"), Some(log::LevelFilter::Debug));
        assert_eq!(parse_level("  WARN "), Some(log::LevelFilter::Warn));
        assert_eq!(parse_level("off"), Some(log::LevelFilter::Off));
        assert_eq!(
            parse_level("chatty"),
            None,
            "an unknown level is left alone"
        );
    }

    #[test]
    fn passwords_do_not_reach_the_log() {
        assert_eq!(
            redacted_url("postgres://root:s3cret@db.internal:5432/shop"),
            "postgres://root:***@db.internal:5432/shop"
        );
        // The last `@` delimits the authority; a password may contain one.
        assert_eq!(
            redacted_url("mysql://root:p@ss@db.internal/shop"),
            "mysql://root:***@db.internal/shop"
        );
        // Nothing to hide, nothing changed.
        assert_eq!(
            redacted_url("postgres://db.internal:5432/shop"),
            "postgres://db.internal:5432/shop"
        );
        assert_eq!(
            redacted_url("postgres://root@db.internal/shop"),
            "postgres://root@db.internal/shop"
        );
        assert_eq!(redacted_url("sqlite::memory:"), "sqlite::memory:");
        assert_eq!(
            redacted_url("sqlite://./data.db?mode=rwc"),
            "sqlite://./data.db?mode=rwc"
        );
    }

    #[tokio::test]
    async fn a_db_section_without_a_url_is_rejected_with_a_useful_message() {
        let err = connect(&DbConfig::default()).await.unwrap_err();
        assert!(err.message.contains("connection url"), "{}", err.message);
        assert!(
            err.client_message().contains("db.url"),
            "{}",
            err.client_message()
        );
    }

    #[tokio::test]
    async fn an_in_memory_database_connects_and_answers_queries() {
        let db = connect_memory().await.unwrap();
        assert!(!has_table(&db, "t_absent").await.unwrap());
    }

    #[tokio::test]
    async fn a_db_section_from_a_config_file_connects() {
        let config = Config::from_value(serde_json::json!({
            "db": { "url": "sqlite::memory:", "sqlLogging": false, "maxConnections": 4 }
        }));

        let db = connect_from_config(&config).await.unwrap().unwrap();
        assert!(!has_table(&db, "t_absent").await.unwrap());
    }

    #[tokio::test]
    async fn no_db_section_means_no_database() {
        let config =
            Config::from_value(serde_json::json!({ "web": { "server": { "port": 19009 } } }));
        assert!(connect_from_config(&config).await.unwrap().is_none());
    }
}
