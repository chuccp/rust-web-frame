//! Caching: one trait, two backends.
//!
//! [`MemoryCache`] is a concurrent, capacity-bounded, TTL-aware map in this process;
//! [`RedisCache`] is the same interface over a shared Redis. Both plug into the container,
//! so a handler asks for [`Cache`](crate::component::Cache) and the deployment decides
//! which one it gets:
//!
//! ```yaml
//! cache:
//!   type: memory      # or `redis`
//!   maxEntries: 10000
//!   ttl: 300          # seconds, the default for entries that do not set their own
//!   url: redis://127.0.0.1:6379    # `type: redis` only
//! ```
//!
//! Values are [`serde_json::Value`] rather than bytes, because that is what a handler has
//! and what a response is made of — the conversion happens once, here, instead of at every
//! call site.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::{Context, Service};
use crate::web::{Error, Result};

/// Config section that holds the cache settings.
pub const CACHE_CONFIG_KEY: &str = "cache";

/// Which backend to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// A map in this process.
    #[default]
    Memory,
    /// A shared Redis.
    Redis,
}

/// Cache configuration, read from the `cache` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CacheConfig {
    /// Which backend to use.
    #[serde(rename = "type")]
    pub backend: Backend,
    /// Maximum entries before the least recently used are evicted. `0` means 10 000.
    pub max_entries: u64,
    /// Default lifetime in seconds for entries that do not set their own. `0` means no
    /// expiry.
    pub ttl: u64,
    /// Redis connection string.
    pub url: String,
    /// Key prefix, so several applications can share one Redis.
    pub prefix: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Memory,
            max_entries: 10_000,
            ttl: 0,
            url: String::from("redis://127.0.0.1:6379"),
            prefix: String::new(),
        }
    }
}

impl CacheConfig {
    /// The effective entry cap.
    #[must_use]
    pub fn effective_max_entries(&self) -> u64 {
        if self.max_entries == 0 {
            10_000
        } else {
            self.max_entries
        }
    }

    /// The default lifetime, or `None` for entries that never expire.
    #[must_use]
    pub fn default_ttl(&self) -> Option<Duration> {
        (self.ttl > 0).then(|| Duration::from_secs(self.ttl))
    }
}

/// What a cache can do.
///
/// Every method takes `&self`: an implementation is shared, and the backends synchronise
/// internally.
#[async_trait]
pub trait Cache: Send + Sync + 'static {
    /// Reads a value.
    async fn get(&self, key: &str) -> Option<Value>;

    /// Writes a value. `ttl` of `None` uses the configured default.
    async fn set(&self, key: &str, value: Value, ttl: Option<Duration>);

    /// Removes a value, returning what was there.
    async fn remove(&self, key: &str) -> Option<Value>;

    /// Writes only when the key is absent, reporting whether it wrote.
    async fn set_if_absent(&self, key: &str, value: Value, ttl: Option<Duration>) -> bool;

    /// Reads, or computes and stores when absent.
    ///
    /// The whole point of a cache in a handler: two concurrent requests for the same cold
    /// key compute once, not twice.
    ///
    /// The loader is boxed because a generic method would make this trait impossible to
    /// hold as a `dyn Cache`; [`CacheExt::get_or_set`] wraps it back into the shape a
    /// caller wants.
    ///
    /// # Errors
    ///
    /// Whatever the loader returns. A failed load is not stored.
    async fn get_or_set_boxed(
        &self,
        key: &str,
        ttl: Option<Duration>,
        load: Box<dyn FnOnce() -> crate::web::BoxFuture<Result<Value>> + Send>,
    ) -> Result<Value>;

    /// How many entries the cache holds, when the backend can say.
    fn entry_count(&self) -> Option<u64>;
}

/// The ergonomic half of [`Cache`]: the generic methods that would otherwise make it
/// impossible to hold as a `dyn Cache`.
///
/// Blanket-implemented, so it is in scope for a concrete cache and for `Arc<dyn Cache>`
/// alike:
///
/// ```no_run
/// # use rust_web_frame::component::{Cache, CacheExt};
/// # use std::sync::Arc;
/// # async fn demo(cache: Arc<dyn Cache>) -> rust_web_frame::web::Result<()> {
/// let value = cache
///     .get_or_set("key", None, || async { Ok(serde_json::json!(42)) })
///     .await?;
/// # let _ = value;
/// # Ok(())
/// # }
/// ```
pub trait CacheExt: Cache {
    /// Reads, or computes and stores when absent.
    ///
    /// # Errors
    ///
    /// Whatever the loader returns.
    fn get_or_set<F, Fut>(
        &self,
        key: &str,
        ttl: Option<Duration>,
        load: F,
    ) -> impl std::future::Future<Output = Result<Value>> + '_
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send + 'static;

    /// Reads a typed value.
    ///
    /// # Errors
    ///
    /// Fails when the stored value does not fit `T` — a bug in the caller, not a cache
    /// miss, so it is reported rather than swallowed.
    fn get_as<T>(&self, key: &str) -> impl std::future::Future<Output = Result<Option<T>>> + '_
    where
        T: serde::de::DeserializeOwned + Send + 'static;

    /// Writes a typed value.
    ///
    /// # Errors
    ///
    /// Fails when `T` cannot be serialised.
    fn set_as<T>(
        &self,
        key: &str,
        value: T,
        ttl: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<()>> + '_
    where
        T: serde::Serialize + Send + 'static;
}

impl<C: Cache + ?Sized> CacheExt for C {
    fn get_or_set<F, Fut>(
        &self,
        key: &str,
        ttl: Option<Duration>,
        load: F,
    ) -> impl std::future::Future<Output = Result<Value>> + '_
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Value>> + Send + 'static,
    {
        let key = key.to_owned();
        async move {
            self.get_or_set_boxed(&key, ttl, Box::new(|| Box::pin(load())))
                .await
        }
    }

    fn get_as<T>(&self, key: &str) -> impl std::future::Future<Output = Result<Option<T>>> + '_
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let key = key.to_owned();
        async move {
            match self.get(&key).await {
                None => Ok(None),
                Some(value) => serde_json::from_value(value).map(Some).map_err(|err| {
                    Error::with_source(500, err)
                        .detail(format!("cache key `{key}` holds another type"))
                }),
            }
        }
    }

    fn set_as<T>(
        &self,
        key: &str,
        value: T,
        ttl: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<()>> + '_
    where
        T: serde::Serialize + Send + 'static,
    {
        let key = key.to_owned();
        async move {
            let value = serde_json::to_value(value).map_err(Error::from)?;
            self.set(&key, value, ttl).await;
            Ok(())
        }
    }
}

// ------------------------------------------------------------------ memory

/// A cache in this process: bounded by entries, with per-entry expiry.
///
/// Backed by `moka`, which is the Rust counterpart of the Go original's `otter` — the same
/// concurrent map with the same eviction and TTL semantics.
pub struct MemoryCache {
    inner: MokaCache<String, Value>,
    ttl: Option<Duration>,
}

impl std::fmt::Debug for MemoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCache")
            .field("entries", &self.inner.entry_count())
            .field("defaultTtl", &self.ttl)
            .finish()
    }
}

impl MemoryCache {
    /// A cache holding at most `max_entries`, whose entries live `ttl` by default.
    #[must_use]
    pub fn new(max_entries: u64, ttl: Option<Duration>) -> Self {
        let mut builder = MokaCache::builder().max_capacity(max_entries);
        if let Some(ttl) = ttl {
            builder = builder.time_to_live(ttl);
        }
        Self {
            inner: builder.build(),
            ttl,
        }
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Option<Value> {
        self.inner.get(key).await
    }

    async fn set(&self, key: &str, value: Value, ttl: Option<Duration>) {
        // `moka`'s lifetime is per-cache, so a per-entry override is not expressible.
        // The entry still expires with the cache's own TTL; a caller that needs a different
        // lifetime for one key should use `RedisCache`, which stores the expiry with the
        // value. Said out loud here rather than silently ignored.
        if let Some(ttl) = ttl
            && self.ttl.is_some_and(|default| ttl != default)
        {
            tracing::debug!(
                key,
                "MemoryCache has one TTL for every entry; the per-entry value was not applied"
            );
        }
        self.inner.insert(key.to_owned(), value).await;
    }

    async fn remove(&self, key: &str) -> Option<Value> {
        let existing = self.inner.get(key).await;
        self.inner.invalidate(key).await;
        existing
    }

    async fn set_if_absent(&self, key: &str, value: Value, _ttl: Option<Duration>) -> bool {
        // `get_with` would run the loader even when the entry exists, so the check is
        // explicit. Two callers can both see a miss; `entry` would close that race, but
        // `moka`'s async API does not expose one.
        if self.inner.contains_key(key) {
            return false;
        }
        self.inner.insert(key.to_owned(), value).await;
        true
    }

    async fn get_or_set_boxed(
        &self,
        key: &str,
        _ttl: Option<Duration>,
        load: Box<dyn FnOnce() -> crate::web::BoxFuture<Result<Value>> + Send>,
    ) -> Result<Value> {
        // `try_get_with` runs the loader exactly once per cold key, even under contention.
        let value = self
            .inner
            .try_get_with(key.to_owned(), load())
            .await
            .map_err(|err: Arc<Error>| (*err).clone())?;
        Ok(value)
    }

    fn entry_count(&self) -> Option<u64> {
        Some(self.inner.entry_count())
    }
}

// ------------------------------------------------------------------- redis

/// A shared cache in Redis.
///
/// Each entry is stored as a JSON string under `{prefix}{key}`, with Redis holding the
/// expiry — so a per-key lifetime works here even though [`MemoryCache`] has one for the
/// whole cache.
pub struct RedisCache {
    client: redis::aio::ConnectionManager,
    prefix: String,
    ttl: Option<Duration>,
}

impl std::fmt::Debug for RedisCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisCache")
            .field("prefix", &self.prefix)
            .field("defaultTtl", &self.ttl)
            .finish()
    }
}

impl RedisCache {
    /// Connects to `url`, storing every key under `prefix`.
    ///
    /// # Errors
    ///
    /// Fails when Redis cannot be reached. A cache the application cannot talk to is
    /// worth failing to start over — the alternative is every request discovering it.
    pub async fn connect(url: &str, prefix: &str, ttl: Option<Duration>) -> Result<Self> {
        let client = redis::Client::open(url)
            .map_err(|err| Error::with_source(500, err).detail("invalid redis url"))?;
        let manager = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|err| {
                Error::with_source(500, err).detail(format!("cannot reach redis at {url}"))
            })?;

        tracing::info!(%url, "redis cache connected");
        Ok(Self {
            client: manager,
            prefix: prefix.to_owned(),
            ttl,
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn expiry(&self, ttl: Option<Duration>) -> Option<Duration> {
        ttl.or(self.ttl)
    }
}

#[async_trait]
impl Cache for RedisCache {
    async fn get(&self, key: &str) -> Option<Value> {
        let mut connection = self.client.clone();
        let raw: Option<String> = redis::cmd("GET")
            .arg(self.full_key(key))
            .query_async(&mut connection)
            .await
            .ok()?;
        raw.and_then(|text| serde_json::from_str(&text).ok())
    }

    async fn set(&self, key: &str, value: Value, ttl: Option<Duration>) {
        let mut connection = self.client.clone();
        let payload = match serde_json::to_string(&value) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!(key, error = %err, "cache value is not serialisable");
                return;
            }
        };

        let mut command = redis::cmd("SET");
        command.arg(self.full_key(key)).arg(payload);
        if let Some(ttl) = self.expiry(ttl) {
            command.arg("PX").arg(ttl.as_millis() as u64);
        }

        if let Err(err) = command.query_async::<()>(&mut connection).await {
            // A cache that is down is not a reason to fail the request it was serving.
            tracing::warn!(key, error = %err, "redis SET failed");
        }
    }

    async fn remove(&self, key: &str) -> Option<Value> {
        let existing = self.get(key).await;
        let mut connection = self.client.clone();
        let _: std::result::Result<(), _> = redis::cmd("DEL")
            .arg(self.full_key(key))
            .query_async(&mut connection)
            .await;
        existing
    }

    async fn set_if_absent(&self, key: &str, value: Value, ttl: Option<Duration>) -> bool {
        let mut connection = self.client.clone();
        let payload = match serde_json::to_string(&value) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!(key, error = %err, "cache value is not serialisable");
                return false;
            }
        };

        // `SET NX` is one round trip and atomic, unlike a GET followed by a SET.
        let mut command = redis::cmd("SET");
        command.arg(self.full_key(key)).arg(payload).arg("NX");
        if let Some(ttl) = self.expiry(ttl) {
            command.arg("PX").arg(ttl.as_millis() as u64);
        }

        match command.query_async::<Option<String>>(&mut connection).await {
            Ok(written) => written.is_some(),
            Err(err) => {
                tracing::warn!(key, error = %err, "redis SET NX failed");
                false
            }
        }
    }

    async fn get_or_set_boxed(
        &self,
        key: &str,
        ttl: Option<Duration>,
        load: Box<dyn FnOnce() -> crate::web::BoxFuture<Result<Value>> + Send>,
    ) -> Result<Value> {
        if let Some(value) = self.get(key).await {
            return Ok(value);
        }

        let value = load().await?;
        self.set(key, value.clone(), ttl).await;
        Ok(value)
    }

    fn entry_count(&self) -> Option<u64> {
        // Counting a shared Redis is not the cache's business, and `DBSIZE` would count
        // another application's keys anyway.
        None
    }
}

// --------------------------------------------------------------- component

/// The cache, registered in the container.
///
/// A handler asks for it by type and talks to the [`Cache`] trait:
///
/// ```no_run
/// # use std::sync::Arc;
/// # use rust_web_frame::component::{Cache, CacheExt};
/// # use rust_web_frame::web::{Request, Result};
/// # async fn handler(req: Request) -> Result<&'static str> {
/// // The backend is registered under its own type, so a handler asks for the trait.
/// let cache = req.context().service::<Arc<dyn Cache>>()?;
/// let hits = cache
///     .get_or_set("hit-count", None, || async { Ok(serde_json::json!(1)) })
///     .await?;
/// # let _ = hits;
/// # Ok("ok")
/// # }
/// ```
pub struct CacheService {
    cache: Arc<dyn Cache>,
}

impl std::fmt::Debug for CacheService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheService").finish_non_exhaustive()
    }
}

impl CacheService {
    /// Wraps a backend.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache }
    }

    /// Builds the backend the configuration asks for.
    ///
    /// # Errors
    ///
    /// Fails when the configured Redis cannot be reached.
    pub async fn from_config(config: &CacheConfig) -> Result<Self> {
        let cache: Arc<dyn Cache> = match config.backend {
            Backend::Memory => Arc::new(MemoryCache::new(
                config.effective_max_entries(),
                config.default_ttl(),
            )),
            Backend::Redis => Arc::new(
                RedisCache::connect(&config.url, &config.prefix, config.default_ttl()).await?,
            ),
        };
        Ok(Self::new(cache))
    }

    /// The backend.
    #[must_use]
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }
}

#[async_trait]
impl Service for CacheService {
    async fn init(&self, ctx: &Context) -> Result<()> {
        // Registered as `Arc<dyn Cache>`: the trait object is what a handler wants, and
        // wrapping it once more is what makes it `Sized` enough to store.
        ctx.register(Arc::new(Arc::clone(&self.cache)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn memory() -> MemoryCache {
        MemoryCache::new(100, Some(Duration::from_secs(60)))
    }

    #[tokio::test]
    async fn a_value_round_trips() {
        let cache = memory().await;
        assert!(cache.get("a").await.is_none());

        cache.set("a", json!({"n": 1}), None).await;
        assert_eq!(cache.get("a").await, Some(json!({"n": 1})));
    }

    #[tokio::test]
    async fn removing_returns_what_was_there() {
        let cache = memory().await;
        cache.set("a", json!(1), None).await;

        assert_eq!(cache.remove("a").await, Some(json!(1)));
        assert!(cache.get("a").await.is_none());
        assert!(cache.remove("a").await.is_none());
    }

    #[tokio::test]
    async fn set_if_absent_only_writes_once() {
        let cache = memory().await;

        assert!(cache.set_if_absent("k", json!("first"), None).await);
        assert!(!cache.set_if_absent("k", json!("second"), None).await);
        assert_eq!(cache.get("k").await, Some(json!("first")));
    }

    #[tokio::test]
    async fn get_or_set_loads_once_and_then_serves_from_memory() {
        let cache = memory().await;
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..3 {
            let loads = Arc::clone(&loads);
            let value = cache
                .get_or_set("k", None, move || async move {
                    loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!("computed"))
                })
                .await
                .unwrap();
            assert_eq!(value, json!("computed"));
        }

        assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_callers_load_a_cold_key_once() {
        let cache = Arc::new(memory().await);
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_set("hot", None, move || async move {
                        loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(json!("value"))
                    })
                    .await
                    .unwrap()
            }));
        }

        for task in tasks {
            assert_eq!(task.await.unwrap(), json!("value"));
        }
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cold key must be computed once, not once per caller"
        );
    }

    #[tokio::test]
    async fn a_loader_error_is_not_cached() {
        let cache = memory().await;

        let failed = cache
            .get_or_set("k", None, || async {
                Err(Error::internal().detail("boom"))
            })
            .await;
        assert!(failed.is_err());
        assert!(
            cache.get("k").await.is_none(),
            "a failed load must not be stored"
        );

        // The next caller gets to try again.
        let ok = cache
            .get_or_set("k", None, || async { Ok(json!("second try")) })
            .await
            .unwrap();
        assert_eq!(ok, json!("second try"));
    }

    #[tokio::test]
    async fn entries_expire() {
        let cache = MemoryCache::new(100, Some(Duration::from_millis(80)));
        cache.set("k", json!("v"), None).await;

        assert_eq!(cache.get("k").await, Some(json!("v")));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            cache.get("k").await.is_none(),
            "the entry should have expired"
        );
    }

    #[tokio::test]
    async fn typed_accessors_report_a_type_mismatch() {
        let cache = memory().await;
        cache.set("k", json!("a string"), None).await;

        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wanted {
            n: i32,
        }

        let mismatch = cache.get_as::<Wanted>("k").await;
        assert!(
            mismatch.is_err(),
            "a stored type that does not fit must be reported"
        );

        cache.set_as("n", Wanted { n: 7 }, None).await.unwrap();
        assert_eq!(
            cache.get_as::<Wanted>("n").await.unwrap(),
            Some(Wanted { n: 7 })
        );
    }

    #[tokio::test]
    async fn the_config_decides_which_backend() {
        let memory = CacheService::from_config(&CacheConfig::default())
            .await
            .unwrap();
        assert!(
            memory.cache().entry_count().is_some(),
            "a memory cache can count itself"
        );

        let config = CacheConfig {
            max_entries: 0,
            ..CacheConfig::default()
        };
        assert_eq!(config.effective_max_entries(), 10_000);
    }

    #[tokio::test]
    async fn the_service_registers_the_backend_for_handlers() {
        let service = CacheService::from_config(&CacheConfig {
            backend: Backend::Memory,
            ..CacheConfig::default()
        })
        .await
        .unwrap();

        let ctx = Context::empty();
        service.init(&ctx).await.unwrap();

        let cache = ctx.service::<Arc<dyn Cache>>().expect("registered");
        cache.set("k", json!(1), None).await;
        assert_eq!(cache.get("k").await, Some(json!(1)));
    }
}
