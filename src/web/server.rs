//! The HTTP server: routing, dispatch and lifecycle.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::{FromRequestParts, Path, Request as AxumRequest};
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::MethodRouter;
use bytes::Bytes;
use tokio::net::TcpListener;

use super::config::ServerConfig;
use super::converter::{Converter, DefaultConverter};
use super::error::{Error, Result};
use super::filter::{Filter, FilterChain};
use super::message::Message;
use super::request::Request;
use super::route::to_axum_path;
use super::routes::Routes;
use crate::core::Context;

/// An HTTP server: a route table, a filter list and a converter, bound to a port.
///
/// Most applications never build one directly — the [`Builder`](crate::Builder) does,
/// and [`Context`] hands out the same registration methods inside a controller's `init`.
pub struct Server {
    config: Arc<ServerConfig>,
    handles: Arc<Routes>,
    filters: Arc<RwLock<Vec<Arc<dyn Filter>>>>,
    converter: Arc<RwLock<Option<Arc<dyn Converter>>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("port", &self.config.port)
            .field("routes", &self.handles.entries().len())
            .field(
                "filters",
                &self.filters.read().map(|f| f.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server with no routes, no filters and the default converter.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            handles: Arc::new(Routes::new()),
            filters: Arc::new(RwLock::new(Vec::new())),
            converter: Arc::new(RwLock::new(None)),
        }
    }

    /// The server's configuration.
    pub fn config(&self) -> &Arc<ServerConfig> {
        &self.config
    }

    /// The route table, for registering routes.
    pub fn handles(&self) -> &Arc<Routes> {
        &self.handles
    }

    /// The port this server listens on.
    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// Appends a filter to the chain.
    pub fn add_filter(&self, filter: Arc<dyn Filter>) {
        self.filters.write().expect("filters poisoned").push(filter);
    }

    /// Replaces the response converter.
    pub fn set_converter(&self, converter: Arc<dyn Converter>) {
        *self.converter.write().expect("converter poisoned") = Some(converter);
    }

    /// The active converter — the custom one, or [`DefaultConverter`].
    pub fn converter(&self) -> Arc<dyn Converter> {
        self.converter
            .read()
            .expect("converter poisoned")
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultConverter))
    }

    /// The active filters.
    pub fn filters(&self) -> Arc<Vec<Arc<dyn Filter>>> {
        Arc::new(self.filters.read().expect("filters poisoned").clone())
    }

    /// Adds routes registered elsewhere — by [`Builder`](crate::Builder) before any
    /// server existed — to this server's table.
    ///
    /// The same `Arc<Route>` is shared, so metadata attached afterwards is still visible
    /// here.
    pub fn merge_routes(&self, routes: &Routes) {
        self.handles.extend(routes.entries());
    }

    /// Checks the route table for mistakes that would otherwise surface as a panic while
    /// binding.
    ///
    /// # Errors
    ///
    /// Fails when two routes claim the same method and path, or when a path template
    /// cannot be parsed by the router.
    pub fn validate(&self) -> Result<()> {
        let mut seen: HashMap<(String, String), String> = HashMap::new();
        let mut probed: HashSet<String> = HashSet::new();
        let mut probe: matchit::Router<()> = matchit::Router::new();

        for route in self.handles.entries() {
            let path = to_axum_path(&join_context_path(&self.config.context_path, route.path()));
            // Several routes may share one path (GET and POST on `/api/users`); the
            // router is asked to parse each distinct path exactly once.
            if probed.insert(path.clone()) {
                probe.insert(path.as_str(), ()).map_err(|err| {
                    Error::new(500, format!("route `{}` is not a valid path", route.path()))
                        .caused_by(err)
                })?;
            }

            for method in route.methods() {
                let key = (path.clone(), method.as_str().to_string());
                if let Some(previous) = seen.insert(key, route.path().to_string()) {
                    return Err(Error::new(
                        500,
                        format!(
                            "{method} {path} is registered twice (`{previous}` and `{}`)",
                            route.path()
                        ),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Builds the axum router for this server.
    ///
    /// Called once, after every controller's `init` has registered its routes.
    pub fn router(self: &Arc<Self>, context: Context) -> Router {
        let filters = self.filters();
        let converter = self.converter();

        let mut by_path: HashMap<String, MethodRouter> = HashMap::new();

        for route in self.handles.entries() {
            let path = to_axum_path(&join_context_path(&self.config.context_path, route.path()));
            let dispatcher = Dispatcher {
                route: route.clone(),
                filters: filters.clone(),
                converter: converter.clone(),
                context: context.clone(),
                config: self.config.clone(),
            };
            let entry = by_path.entry(path.clone()).or_default();
            for method in route.methods() {
                *entry = std::mem::take(entry).on(method_filter(method.clone()), {
                    let dispatcher = dispatcher.clone();
                    move |req: AxumRequest| dispatch(dispatcher.clone(), req)
                });
            }
            let allowed = allowed_header(route.methods());
            *entry = std::mem::take(entry).fallback(move || {
                let allowed = allowed.clone();
                async move { method_not_allowed(allowed) }
            });
        }

        let mut router = Router::new();
        for (path, method_router) in by_path {
            router = router.route(&path, method_router);
        }

        let fallback_state = FallbackState {
            config: self.config.clone(),
            converter: converter.clone(),
            context,
        };
        router.fallback(move |req: AxumRequest| {
            let state = fallback_state.clone();
            async move { fallback(state, req).await }
        })
    }

    /// Binds the port and serves until the context is cancelled.
    pub async fn serve(self: &Arc<Self>, context: Context) -> Result<()> {
        let router = self.router(context.clone());
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.port));
        let listener = TcpListener::bind(addr).await.map_err(|err| {
            Error::with_source(500, err).detail(format!("cannot bind port {}", self.config.port))
        })?;

        tracing::info!(port = self.config.port, "server listening");

        let cancel = context.cancellation_token();
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .map_err(Error::from)?;

        Ok(())
    }
}

/// Everything the dispatcher needs for one route.
#[derive(Clone)]
struct Dispatcher {
    route: Arc<super::route::Route>,
    filters: Arc<Vec<Arc<dyn Filter>>>,
    converter: Arc<dyn Converter>,
    context: Context,
    config: Arc<ServerConfig>,
}

/// Runs the filter chain for a matched route and converts the result.
async fn dispatch(state: Dispatcher, req: AxumRequest) -> axum::response::Response {
    let (mut parts, body) = req.into_parts();

    let params = match Path::<HashMap<String, String>>::from_request_parts(&mut parts, &()).await {
        Ok(Path(params)) => params,
        Err(_) => HashMap::new(),
    };
    let upgrade = axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .ok();

    let limit = state.config.effective_max_body_size();
    let body: Bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let error = if limit != usize::MAX {
                Error::new(413, "request body too large")
                    .detail(format!("body exceeds {limit} bytes"))
            } else {
                Error::from(err)
            };
            return state
                .converter
                .convert(Err(error), &Request::for_test("/"))
                .await;
        }
    };

    let request = Request::new(
        parts,
        body,
        params,
        state.route.path().to_string(),
        state.route.meta().clone(),
        state.config.clone(),
        state.context.clone(),
        upgrade,
    );

    let mut chain = FilterChain::new(state.filters.clone(), state.route.handler().clone());
    let result = chain.next(request.clone()).await;
    state.converter.convert(result, &request).await
}

#[derive(Clone)]
struct FallbackState {
    config: Arc<ServerConfig>,
    converter: Arc<dyn Converter>,
    context: Context,
}

/// Routes requests that matched no route: static files, the SPA fallback, then 404.
async fn fallback(state: FallbackState, req: AxumRequest) -> axum::response::Response {
    // The fallback bypasses the filter chain, so build a request by hand.
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, state.config.effective_max_body_size())
        .await
        .unwrap_or_default();
    let request = Request::new(
        parts.clone(),
        body,
        HashMap::new(),
        String::new(),
        Arc::new(super::route::HandlerMeta::new()),
        state.config.clone(),
        state.context.clone(),
        None,
    );

    let mut path = parts.uri.path().to_string();
    if !state.config.context_path.is_empty()
        && let Some(stripped) = path.strip_prefix(&state.config.context_path)
    {
        path = stripped.to_string();
    }

    // 1. A file from one of the configured static directories.
    for directory in &state.config.locations {
        if let Ok(response) = super::routes::serve_file(directory, &path).await {
            return state.converter.convert(Ok(response), &request).await;
        }
    }

    // 2. The SPA fallback page, for browser navigations only.
    if accepts_html(&request)
        && let Some(page) = &state.config.page404
        && tokio::fs::try_exists(page).await.unwrap_or(false)
        && let Ok(bytes) = tokio::fs::read(page).await
    {
        return axum::response::Response::builder()
            .status(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    // 3. Nothing matched.
    let error = Error::not_found().detail(format!(
        "no route matches {} {}",
        request.method(),
        parts.uri.path()
    ));
    state.converter.convert(Err(error), &request).await
}

fn accepts_html(req: &Request) -> bool {
    req.header("accept")
        .is_some_and(|accept| accept.contains("text/html"))
}

/// Renders the `Allow` header for a set of methods.
fn allowed_header(methods: &[Method]) -> String {
    let mut names: Vec<&str> = methods.iter().map(Method::as_str).collect();
    if names.contains(&"GET") && !names.contains(&"HEAD") {
        names.push("HEAD");
    }
    names.join(", ")
}

/// The envelope sent for a request whose path matched but whose method did not.
fn method_not_allowed(allowed: String) -> axum::response::Response {
    let message = Message {
        code: 405,
        data: serde_json::Value::Null,
        msg: String::from("method not allowed"),
        ty: String::new(),
    };
    let body = serde_json::to_string(&message).unwrap_or_else(|_| String::from("{}"));
    axum::response::Response::builder()
        .status(405)
        .header("content-type", "application/json; charset=utf-8")
        .header("allow", allowed)
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response())
}

fn method_filter(method: Method) -> axum::routing::MethodFilter {
    use axum::routing::MethodFilter;
    match method {
        Method::GET => MethodFilter::GET,
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::DELETE => MethodFilter::DELETE,
        Method::PATCH => MethodFilter::PATCH,
        Method::HEAD => MethodFilter::HEAD,
        Method::OPTIONS => MethodFilter::OPTIONS,
        Method::TRACE => MethodFilter::TRACE,
        Method::CONNECT => MethodFilter::CONNECT,
        _ => MethodFilter::GET,
    }
}

/// Joins a context path prefix with a route path, collapsing the duplicate slash.
pub(crate) fn join_context_path(context_path: &str, route_path: &str) -> String {
    if context_path.is_empty() {
        return route_path.to_string();
    }
    let prefix = context_path.trim_end_matches('/');
    if route_path == "/" {
        return prefix.to_string();
    }
    format!("{prefix}{route_path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_paths_join_without_doubling_slashes() {
        assert_eq!(join_context_path("", "/users"), "/users");
        assert_eq!(join_context_path("/api", "/users"), "/api/users");
        assert_eq!(join_context_path("/api/", "/users"), "/api/users");
        assert_eq!(join_context_path("/api", "/"), "/api");
    }

    #[test]
    fn method_filters_cover_everything_the_framework_routes() {
        assert_eq!(method_filter(Method::GET), axum::routing::MethodFilter::GET);
        assert_eq!(
            method_filter(Method::TRACE),
            axum::routing::MethodFilter::TRACE
        );
        assert_eq!(
            method_filter(Method::CONNECT),
            axum::routing::MethodFilter::CONNECT
        );
    }
}
