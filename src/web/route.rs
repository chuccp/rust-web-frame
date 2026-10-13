//! Route definitions and per-route metadata.
//!
//! The headline idea of the framework: a route declares *what it is* via
//! [`RouteHandle::with_meta`], and one [`Filter`](super::Filter) decides *what to do
//! about it* by asking [`Request::has_meta`](super::Request::has_meta).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::http::Method;
use serde_json::Value;

use super::filter::Handler;
use crate::web::BoxFuture;

/// Key/value metadata attached to a route, readable per request through
/// [`Request::has_meta`](super::Request::has_meta).
#[derive(Debug, Default)]
pub struct HandlerMeta {
    values: RwLock<HashMap<String, Value>>,
}

impl HandlerMeta {
    /// An empty metadata set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key` to `value`.
    pub fn insert(&self, key: impl Into<String>, value: impl Into<Value>) {
        let mut values = self.values.write().expect("metadata lock poisoned");
        values.insert(key.into(), value.into());
    }

    /// Sets every key in `keys` to `true`.
    pub fn insert_keys<I, S>(&self, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut values = self.values.write().expect("metadata lock poisoned");
        for key in keys {
            values.insert(key.into(), Value::Bool(true));
        }
    }

    /// Reads a metadata value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        self.values
            .read()
            .expect("metadata lock poisoned")
            .get(key)
            .cloned()
    }

    /// Reports whether `key` is present, whatever its value.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.values
            .read()
            .expect("metadata lock poisoned")
            .contains_key(key)
    }

    /// Reports whether **any** of `keys` is present.
    #[must_use]
    pub fn contains_any<I, S>(&self, keys: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let values = self.values.read().expect("metadata lock poisoned");
        keys.into_iter()
            .any(|key| values.contains_key(key.as_ref()))
    }

    /// Reports whether `key` exists *and* equals `value`.
    #[must_use]
    pub fn contains_entry(&self, key: &str, value: &Value) -> bool {
        self.values
            .read()
            .expect("metadata lock poisoned")
            .get(key)
            .is_some_and(|stored| stored == value)
    }

    /// A snapshot of all metadata. Used by logging and tests.
    #[must_use]
    pub fn snapshot(&self) -> HashMap<String, Value> {
        self.values.read().expect("metadata lock poisoned").clone()
    }

    /// Reports whether the route is empty of metadata.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values
            .read()
            .expect("metadata lock poisoned")
            .is_empty()
    }
}

/// A predicate over a route's [`HandlerMeta`].
///
/// A `MetaOption` does double duty: [`RouteHandle::with_meta`] writes it onto a route,
/// and [`Request::has_meta`](super::Request::has_meta) tests whether a route satisfies it.
/// Declare the option once, next to the filter that consumes it:
///
/// ```
/// use rust_web_frame::web::MetaOption;
///
/// pub fn require_auth() -> MetaOption {
///     MetaOption::value("require_auth", true)
/// }
///
/// pub fn require_permission(permission: &str) -> MetaOption {
///     MetaOption::value("require_permission", permission)
/// }
///
/// pub fn public() -> MetaOption {
///     MetaOption::key(["skip_auth"])
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum MetaOption {
    /// Marks each key as `true`. Matches when *any* of the keys is present.
    Key(Vec<String>),
    /// Sets one key to a value. Matches only on an exact value.
    Value(String, Value),
}

impl MetaOption {
    /// Marks each of `keys` as `true` on the route.
    ///
    /// ```
    /// # use rust_web_frame::web::MetaOption;
    /// let one = MetaOption::key(["login"]);
    /// let several = MetaOption::key(["login", "admin"]);
    /// ```
    pub fn key<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Key(keys.into_iter().map(Into::into).collect())
    }

    /// Sets `key` to `value` on the route.
    ///
    /// ```
    /// # use rust_web_frame::web::MetaOption;
    /// let flag = MetaOption::value("require_auth", true);
    /// let scoped = MetaOption::value("require_permission", "admin:create_user");
    /// ```
    pub fn value(key: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Value(key.into(), value.into())
    }

    /// Writes this option onto `meta`.
    pub fn apply(&self, meta: &HandlerMeta) {
        match self {
            Self::Key(keys) => meta.insert_keys(keys.iter().cloned()),
            Self::Value(key, value) => meta.insert(key.clone(), value.clone()),
        }
    }

    /// Tests whether `meta` satisfies this option.
    #[must_use]
    pub fn matches(&self, meta: &HandlerMeta) -> bool {
        match self {
            Self::Key(keys) => meta.contains_any(keys.iter()),
            Self::Value(key, value) => meta.contains_entry(key, value),
        }
    }
}

impl std::fmt::Display for MetaOption {
    /// What the option asks for, for a log line or an error: `` `login` `` or
    /// `` `require_permission` = "admin:create_user" ``.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(keys) => write!(f, "`{}`", keys.join("` or `")),
            Self::Value(key, value) => write!(f, "`{key}` = {value}"),
        }
    }
}

/// What kind of endpoint a route serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RouteKind {
    /// A handler returning a [`Response`](super::Response).
    #[default]
    Normal,
    /// A WebSocket upgrade endpoint.
    WebSocket,
    /// A Server-Sent Events endpoint.
    Sse,
    /// A static file mount.
    Static,
}

/// A registered route: its path template, the HTTP methods it answers, its handler and
/// its metadata.
pub struct Route {
    path: String,
    methods: Vec<Method>,
    handler: Handler,
    meta: Arc<HandlerMeta>,
    kind: RouteKind,
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("path", &self.path)
            .field("methods", &self.methods)
            .field("kind", &self.kind)
            .field("meta", &self.meta.snapshot())
            .finish_non_exhaustive()
    }
}

impl Route {
    pub(crate) fn new(path: impl Into<String>, methods: Vec<Method>, handler: Handler) -> Self {
        Self {
            path: path.into(),
            methods,
            handler,
            meta: Arc::new(HandlerMeta::new()),
            kind: RouteKind::Normal,
        }
    }

    pub(crate) fn of_kind(mut self, kind: RouteKind) -> Self {
        self.kind = kind;
        self
    }

    /// The path template as registered, in Gin syntax (`/users/:id`, `/static/*filepath`).
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The HTTP methods this route answers.
    #[must_use]
    pub fn methods(&self) -> &[Method] {
        &self.methods
    }

    /// The endpoint handler.
    #[must_use]
    pub fn handler(&self) -> &Handler {
        &self.handler
    }

    /// The route's metadata store.
    #[must_use]
    pub fn meta(&self) -> &HandlerMeta {
        &self.meta
    }

    /// The metadata as a shared handle, for the request that has to outlive the borrow.
    pub(crate) fn shared_meta(&self) -> &Arc<HandlerMeta> {
        &self.meta
    }

    /// What kind of endpoint this is.
    #[must_use]
    pub fn kind(&self) -> RouteKind {
        self.kind
    }

    /// Whether `method` is one of the route's methods.
    #[must_use]
    pub fn accepts(&self, method: &Method) -> bool {
        self.methods.iter().any(|m| m == method)
    }
}

/// A handle to a route that is already in the server's table.
///
/// Registration returns one so that metadata can be attached fluently while the server
/// keeps its own reference:
///
/// ```
/// # use rust_web_frame::core::Context;
/// # use rust_web_frame::web::{handler, MetaOption};
/// # fn require_auth() -> MetaOption { MetaOption::value("require_auth", true) }
/// # fn require_permission(p: &str) -> MetaOption { MetaOption::value("require_permission", p) }
/// # fn routes(ctx: &Context) {
/// ctx.get("/api/profile", handler(|_| async { Ok("profile") }))
///     .with_meta(require_auth());
///
/// ctx.post("/api/admin/users", handler(|_| async { Ok("created") }))
///     .with_meta(require_auth())
///     .with_meta(require_permission("admin:create_user"));
/// # }
/// ```
///
/// The handle dereferences to [`Route`], so `route.path()`, `route.kind()` and friends
/// work directly on it.
#[derive(Debug, Clone)]
pub struct RouteHandle(Arc<Route>);

impl RouteHandle {
    pub(crate) fn new(route: Arc<Route>) -> Self {
        Self(route)
    }

    /// Attaches metadata to the route, consuming the handle so calls chain.
    ///
    /// The metadata is shared with the server's copy of the route, so a filter sees it
    /// on the next request — no rebuild needed. The handle is returned so calls can
    /// chain; discarding it is equally fine.
    #[allow(clippy::return_self_not_must_use)]
    pub fn with_meta(self, option: MetaOption) -> Self {
        option.apply(self.meta());
        self
    }

    /// Attaches several metadata options.
    #[allow(clippy::return_self_not_must_use)]
    pub fn with_metas(self, options: impl IntoIterator<Item = MetaOption>) -> Self {
        for option in options {
            option.apply(self.meta());
        }
        self
    }

    /// The route's metadata store.
    #[must_use]
    pub fn meta(&self) -> &HandlerMeta {
        &self.0.meta
    }

    /// The underlying route.
    #[must_use]
    pub fn route(&self) -> &Arc<Route> {
        &self.0
    }

    /// Consumes the handle, yielding the shared route.
    #[must_use]
    pub fn into_route(self) -> Arc<Route> {
        self.0
    }

    /// Registers an extra endpoint that runs before this route's handler.
    ///
    /// Handy for one-off preprocessing without touching the global filter list.
    #[allow(clippy::return_self_not_must_use)]
    pub fn before<F>(self, preprocess: F) -> Self
    where
        F: Fn(crate::web::Request) -> BoxFuture<Result<(), super::Error>> + Send + Sync + 'static,
    {
        let preprocess = Arc::new(preprocess);
        let inner = self.0;
        let handler = inner.handler.clone();
        let wrapped: Handler = Arc::new(move |req| {
            let preprocess = preprocess.clone();
            let handler = handler.clone();
            Box::pin(async move {
                preprocess(req.clone()).await?;
                handler(req).await
            })
        });
        Self(Arc::new(Route::new(
            inner.path.clone(),
            inner.methods.clone(),
            wrapped,
        )))
    }
}

impl std::ops::Deref for RouteHandle {
    type Target = Route;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Translates a Gin path template into the syntax axum's router expects.
///
/// * `/users/:id` → `/users/{id}`
/// * `/static/*filepath` → `/static/{*filepath}`
/// * `/users/{id}` is passed through unchanged, so either style may be written.
#[must_use]
pub(crate) fn to_axum_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 4);
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        out.push('/');
        match segment.strip_prefix(':') {
            Some(name) => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
            // Gin spells the catch-all `*name`, axum spells it `{*name}`.
            None => match segment.strip_prefix('*') {
                Some(name) => {
                    out.push_str("{*");
                    out.push_str(name);
                    out.push('}');
                }
                None => out.push_str(segment),
            },
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gin_paths_translate_to_axum_paths() {
        assert_eq!(to_axum_path("/users/:id"), "/users/{id}");
        assert_eq!(to_axum_path("/static/*filepath"), "/static/{*filepath}");
        assert_eq!(to_axum_path("/users/{id}"), "/users/{id}");
        assert_eq!(to_axum_path("/"), "/");
        assert_eq!(to_axum_path("/a/b/:c/d"), "/a/b/{c}/d");
        // A trailing slash names the same route.
        assert_eq!(to_axum_path("/users/"), "/users");
        assert_eq!(to_axum_path(""), "/");
    }

    #[test]
    fn a_value_option_matches_only_the_exact_value() {
        let option = MetaOption::value("permission", "admin:create");
        let meta = HandlerMeta::new();
        option.apply(&meta);

        assert!(option.matches(&meta));
        assert!(meta.contains_entry("permission", &Value::from("admin:create")));
        assert!(!meta.contains_entry("permission", &Value::from("admin:delete")));
        assert!(!MetaOption::value("permission", "admin:delete").matches(&meta));
    }

    #[test]
    fn a_key_option_matches_any_of_its_keys() {
        let meta = HandlerMeta::new();
        MetaOption::key(["skip_auth"]).apply(&meta);

        assert!(MetaOption::key(["skip_auth"]).matches(&meta));
        assert!(MetaOption::key(["skip_auth", "other"]).matches(&meta));
        assert!(!MetaOption::key(["require_auth"]).matches(&meta));
    }

    #[test]
    fn metadata_is_shared_with_every_handle_to_the_route() {
        let route = Arc::new(Route::new(
            "/x",
            vec![Method::GET],
            Arc::new(|_| Box::pin(async { Ok(super::super::Response::empty()) })),
        ));
        let handle = RouteHandle::new(route.clone());
        handle
            .clone()
            .with_meta(MetaOption::value("a", 1))
            .with_meta(MetaOption::key(["b"]));

        assert!(route.meta.contains_entry("a", &Value::from(1)));
        assert!(route.meta.contains_key("b"));
    }
}
