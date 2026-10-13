//! The dependency-injection container.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::Database;
use crate::model::Page;
use crate::web::Server as WebServer;
use crate::web::{
    Error, Handler, Request, Response, Result, RouteHandle, ServerConfig, SseStream,
    WebSocketStream, code, handler,
};

/// A registered component, keyed by its concrete type.
#[derive(Clone)]
struct Registered {
    value: Arc<dyn Any + Send + Sync>,
    name: &'static str,
}

struct Inner {
    config: Arc<Config>,
    server: RwLock<Arc<WebServer>>,
    /// Shared with every context forked off this one, so a second port sees the same
    /// components.
    registry: Arc<RwLock<HashMap<TypeId, Registered>>>,
    database: Arc<RwLock<Option<Database>>>,
    cancel: CancellationToken,
}

/// The dependency-injection container, and the handle a component uses to reach the
/// framework.
///
/// `Context` is cheap to clone — every clone shares one registry — and it is the same
/// object at startup and at request time, so a route registered in `init` can look a
/// service up again while serving:
///
/// ```no_run
/// # use rust_web_frame::core::{Context, Service};
/// # use rust_web_frame::web::{handler, Result, Request, Response};
/// # use async_trait::async_trait;
/// # #[derive(Default)] struct UserService;
/// # impl UserService { fn count(&self) -> usize { 0 } }
/// # #[derive(Default)] struct Controller;
/// # #[async_trait]
/// # impl Service for Controller {
/// # async fn init(&self, ctx: &Context) -> Result<()> {
/// // Move a handle into the closure; the context is shared, not copied.
/// let owned = ctx.clone();
/// ctx.get("/api/count", handler(move |_req: Request| {
///     let users = owned.service::<UserService>().expect("registered in init");
///     async move { Ok(Response::json(serde_json::json!(users.count()))) }
/// }));
/// # Ok(())
/// # }
/// # }
/// ```
#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field(
                "services",
                &self.inner.registry.read().map(|r| r.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl Context {
    /// A context serving through `server`.
    pub fn new(config: Config, server: Arc<WebServer>) -> Self {
        Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                server: RwLock::new(server),
                registry: Arc::new(RwLock::new(HashMap::new())),
                database: Arc::new(RwLock::new(None)),
                cancel: CancellationToken::new(),
            }),
        }
    }

    /// A context with no configuration and a throwaway server. Used in tests.
    pub fn empty() -> Self {
        let server = Arc::new(WebServer::new(ServerConfig::default()));
        Self::new(Config::empty(), server)
    }

    /// A context that shares this one's registry and configuration but serves through a
    /// different [`Server`](crate::web::Server) — how a second port gets its own routes.
    pub fn with_server(&self, server: Arc<WebServer>) -> Self {
        Self {
            inner: Arc::new(Inner {
                config: self.inner.config.clone(),
                server: RwLock::new(server),
                registry: Arc::clone(&self.inner.registry),
                database: Arc::clone(&self.inner.database),
                cancel: self.inner.cancel.clone(),
            }),
        }
    }

    // ------------------------------------------------------------- framework

    /// The application configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The application configuration, as a shared handle — for moving into a task or a
    /// long-lived closure.
    #[must_use]
    pub fn config_arc(&self) -> Arc<Config> {
        Arc::clone(&self.inner.config)
    }

    /// The server this context registers routes on.
    pub fn server(&self) -> Arc<WebServer> {
        self.inner.server.read().expect("server poisoned").clone()
    }

    /// A token that is cancelled when the application shuts down.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Asks the application to stop. In-flight requests are drained first.
    pub fn shutdown(&self) {
        self.inner.cancel.cancel();
    }

    /// Reports whether shutdown has been requested.
    pub fn is_shutting_down(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    /// Spawns a task that outlives the current request. Panics inside it are logged
    /// rather than killing the process.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(async move {
            if let Err(err) = tokio::spawn(future).await {
                tracing::error!(error = %err, "spawned task failed");
            }
        })
    }

    // ------------------------------------------------------------------- DI

    /// Registers a component under its concrete type, replacing any earlier registration
    /// of the same type.
    pub fn register<T: Send + Sync + 'static>(&self, value: Arc<T>) {
        self.register_dyn(
            TypeId::of::<T>(),
            std::any::type_name::<T>(),
            value as Arc<dyn Any + Send + Sync>,
        );
    }

    /// Registers a component whose type has already been erased.
    pub(crate) fn register_dyn(
        &self,
        type_id: TypeId,
        name: &'static str,
        value: Arc<dyn Any + Send + Sync>,
    ) {
        if self
            .inner
            .registry
            .read()
            .expect("registry poisoned")
            .contains_key(&type_id)
        {
            tracing::debug!(service = name, "re-registered, the later value wins");
        }
        self.inner
            .registry
            .write()
            .expect("registry poisoned")
            .insert(type_id, Registered { value, name });
    }

    /// Looks a component up by type.
    ///
    /// ```no_run
    /// # use rust_web_frame::core::Context;
    /// # #[derive(Default)] struct UserModel;
    /// # let ctx = Context::empty();
    /// let users = ctx.service::<UserModel>().expect("registered at startup");
    /// ```
    pub fn service<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.try_get::<T>().ok_or_else(|| {
            let name = std::any::type_name::<T>();
            Error::new(code::INTERNAL_ERROR, format!("{name} is not registered")).detail(format!(
                "{name} is not registered — register it with `builder.service(...)` \
                 before the component that needs it"
            ))
        })
    }

    /// Looks a component up by type, panicking when it is absent. Use inside `init`,
    /// where a missing dependency is a programming error rather than a runtime condition.
    pub fn must_service<T: Send + Sync + 'static>(&self) -> Arc<T> {
        self.service::<T>().unwrap_or_else(|err| {
            panic!("{err}");
        })
    }

    /// Looks a component up by type, returning `None` when it is absent.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.inner
            .registry
            .read()
            .expect("registry poisoned")
            .get(&TypeId::of::<T>())
            .and_then(|entry| entry.value.clone().downcast::<T>().ok())
    }

    /// Alias of [`service`](Self::service) — models and services share one registry, so
    /// this is the name that reads best at a call site that wants a model.
    pub fn model<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.service::<T>()
    }

    /// Alias of [`must_service`](Self::must_service).
    pub fn must_model<T: Send + Sync + 'static>(&self) -> Arc<T> {
        self.must_service::<T>()
    }

    /// Reports whether a component of this type has been registered.
    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.inner
            .registry
            .read()
            .expect("registry poisoned")
            .contains_key(&TypeId::of::<T>())
    }

    /// The names of every registered component, for diagnostics.
    pub fn registered_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self
            .inner
            .registry
            .read()
            .expect("registry poisoned")
            .values()
            .map(|entry| entry.name)
            .collect();
        names.sort_unstable();
        names
    }

    // ------------------------------------------------------------ database

    /// Records the application's default database, so [`db`](Self::db) works without a
    /// model group.
    pub fn set_database(&self, db: Database) {
        *self.inner.database.write().expect("database poisoned") = Some(db);
    }

    /// The default database, when one is configured.
    pub fn db(&self) -> Result<Database> {
        self.inner
            .database
            .read()
            .expect("database poisoned")
            .clone()
            .ok_or_else(|| {
                Error::new(code::INTERNAL_ERROR, "no database is configured")
                    .detail("add a `db` section to the config file")
            })
    }

    /// The default database, or `None` when the application runs without one.
    pub fn try_db(&self) -> Option<Database> {
        self.inner
            .database
            .read()
            .expect("database poisoned")
            .clone()
    }

    // -------------------------------------------------------------- routing

    /// Registers a `GET` route.
    pub fn get(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().get(path, endpoint)
    }

    /// Registers a `POST` route.
    pub fn post(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().post(path, endpoint)
    }

    /// Registers a `PUT` route.
    pub fn put(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().put(path, endpoint)
    }

    /// Registers a `DELETE` route.
    pub fn delete(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().delete(path, endpoint)
    }

    /// Registers a `PATCH` route.
    pub fn patch(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().patch(path, endpoint)
    }

    /// Registers a route answering every HTTP method.
    pub fn any(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.server().routes().any(path, endpoint)
    }

    /// Registers a Server-Sent Events endpoint.
    ///
    /// ```no_run
    /// # use rust_web_frame::core::Context;
    /// # use rust_web_frame::web::{Result, SseStream};
    /// # fn demo(ctx: &Context) {
    /// ctx.sse("/events", |stream: SseStream| Box::pin(async move {
    ///     stream.send("greeting", "hello");
    ///     Ok(())
    /// }));
    /// # }
    /// ```
    pub fn sse<F>(&self, path: &str, endpoint: F) -> RouteHandle
    where
        F: Fn(SseStream) -> crate::web::BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        self.server().routes().sse(path, endpoint)
    }

    /// Registers a WebSocket endpoint.
    pub fn websocket<F>(&self, path: &str, endpoint: F) -> RouteHandle
    where
        F: Fn(WebSocketStream) -> crate::web::BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        self.server().routes().websocket(path, endpoint)
    }

    /// Serves a directory of static files.
    pub fn static_files(
        &self,
        path: &str,
        directory: impl Into<std::path::PathBuf>,
    ) -> RouteHandle {
        self.server().routes().static_files(path, directory)
    }

    /// Registers a redirect from one path to another.
    pub fn redirect(&self, path: &str, location: impl Into<String>) -> RouteHandle {
        let location = location.into();
        self.get(
            path,
            handler(move |_req: Request| {
                let location = location.clone();
                async move { Ok(Response::redirect(location)) }
            }),
        )
    }

    /// Registers a health check at `/healthz` (and `/api/healthz` when a context path is
    /// configured), answering with uptime and registered component counts.
    pub fn health_check(&self) -> RouteHandle {
        let ctx = self.clone();
        let started = std::time::Instant::now();
        self.get(
            "/healthz",
            handler(move |_req: Request| {
                let ctx = ctx.clone();
                async move {
                    Ok(Response::json(serde_json::json!({
                        "status": "ok",
                        "uptimeSeconds": started.elapsed().as_secs(),
                        "components": ctx.registered_names().len(),
                    })))
                }
            }),
        )
    }

    /// The page size defaults used by [`Request::page`](crate::web::Request::page).
    pub fn default_page(&self) -> Page {
        Page::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{MetaOption, handler};

    #[derive(Debug, PartialEq)]
    struct Greeter {
        greeting: String,
    }

    #[test]
    fn services_round_trip_by_type() {
        let ctx = Context::empty();
        assert!(!ctx.contains::<Greeter>());
        assert!(ctx.service::<Greeter>().is_err());

        ctx.register(Arc::new(Greeter {
            greeting: String::from("hi"),
        }));

        assert!(ctx.contains::<Greeter>());
        let greeter = ctx.service::<Greeter>().unwrap();
        assert_eq!(greeter.greeting, "hi");
        assert_eq!(ctx.model::<Greeter>().unwrap().greeting, "hi");
    }

    #[test]
    fn clones_share_one_registry() {
        let ctx = Context::empty();
        let clone = ctx.clone();
        clone.register(Arc::new(Greeter {
            greeting: String::from("shared"),
        }));
        assert_eq!(ctx.service::<Greeter>().unwrap().greeting, "shared");
    }

    #[test]
    fn a_bad_lookup_explains_itself() {
        let ctx = Context::empty();
        let err = ctx.service::<Greeter>().unwrap_err();
        assert!(err.client_message().contains("Greeter"));
        assert!(err.client_message().contains("not registered"));
    }

    #[test]
    fn registered_names_are_sorted_and_complete() {
        let ctx = Context::empty();
        ctx.register(Arc::new(Greeter {
            greeting: String::new(),
        }));
        ctx.register(Arc::new(7u64));
        let names = ctx.registered_names();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n.ends_with("Greeter")));
    }

    #[test]
    fn the_last_registration_of_a_type_wins() {
        let ctx = Context::empty();
        ctx.register(Arc::new(Greeter {
            greeting: String::from("first"),
        }));
        ctx.register(Arc::new(Greeter {
            greeting: String::from("second"),
        }));
        assert_eq!(ctx.service::<Greeter>().unwrap().greeting, "second");
    }

    #[test]
    fn routes_land_in_the_servers_table() {
        let ctx = Context::empty();
        ctx.get("/a", handler(|_| async { Ok("a") }));
        let route = ctx.post("/b", handler(|_| async { Ok("b") }));
        route.with_meta(MetaOption::value("require_auth", true));

        let routes = ctx.server().routes().entries();
        assert_eq!(routes.len(), 2);
        assert!(routes[1].meta().contains_key("require_auth"));
    }

    #[test]
    fn shutdown_is_observable() {
        let ctx = Context::empty();
        assert!(!ctx.is_shutting_down());
        ctx.shutdown();
        assert!(ctx.is_shutting_down());
    }

    #[test]
    fn a_second_server_shares_the_registry() {
        let ctx = Context::empty();
        ctx.register(Arc::new(Greeter {
            greeting: String::from("x"),
        }));

        let other = Arc::new(WebServer::new(ServerConfig::on_port(9999)));
        let forked = ctx.with_server(other);
        assert_eq!(forked.service::<Greeter>().unwrap().greeting, "x");
        assert_eq!(forked.server().port(), 9999);
        assert_eq!(ctx.server().port(), 19009);
    }

    #[tokio::test]
    async fn databases_can_be_attached_and_reused() {
        let ctx = Context::empty();
        assert!(ctx.db().is_err());

        let db = crate::db::connect_memory().await.unwrap();
        ctx.set_database(db);
        assert!(ctx.db().is_ok());
    }
}
