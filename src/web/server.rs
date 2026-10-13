//! The HTTP server: routing, dispatch and lifecycle.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::extract::{FromRequestParts, Path, Request as AxumRequest};
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::MethodRouter;
use tower_http::catch_panic::{CatchPanicLayer, ResponseForPanic};
use tower_http::cors::{Any, CorsLayer};

use super::config::ServerConfig;
use super::converter::{Converter, DefaultConverter};
use super::error::{Error, Result};
use super::filter::{Filter, FilterChain};
use super::message::Message;
use super::request::{Body, Request};
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
    ///
    /// The configuration is normalised here, so every later reader sees a route prefix
    /// in one shape.
    pub fn new(mut config: ServerConfig) -> Self {
        config.normalise();
        Self {
            config: Arc::new(config),
            handles: Arc::new(Routes::new()),
            filters: Arc::new(RwLock::new(Vec::new())),
            converter: Arc::new(RwLock::new(None)),
        }
    }

    /// The server's configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// The route table, for registering routes.
    #[must_use]
    pub fn routes(&self) -> &Routes {
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

    /// Checks the route table and the configuration for mistakes that would otherwise
    /// surface as a panic while binding, or as a silently missing header.
    ///
    /// # Errors
    ///
    /// Fails on a duplicate route, an unparsable path template, or a CORS origin or
    /// method that is not a valid header value.
    pub fn validate_config(&self) -> Result<()> {
        if let Some(cors) = self
            .config
            .cors
            .as_ref()
            .filter(|cors| cors.is_configured())
        {
            let _ = cors_layer(cors)?;
        }
        Ok(())
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
        // Several routes share one path (`GET /users` and `POST /users`), and the `Allow`
        // header on a 405 has to list all of them — not just whichever was registered last.
        let mut methods_by_path: HashMap<String, Vec<Method>> = HashMap::new();

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
            methods_by_path
                .entry(path)
                .or_default()
                .extend(route.methods().iter().cloned());
        }

        // The fallback goes on once per path, with every method the path answers.
        for (path, entry) in by_path.iter_mut() {
            let allowed =
                allowed_header(methods_by_path.get(path).map(Vec::as_slice).unwrap_or(&[]));
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
        let router = router
            .fallback(move |req: AxumRequest| {
                let state = fallback_state.clone();
                async move { fallback(state, req).await }
            })
            // Applied in order, so the last one added is the outermost.
            .layer(CatchPanicLayer::custom(PanicHandler));

        // CORS goes outside everything, because a preflight `OPTIONS` matches no route
        // and has to be answered before routing decides it is a 405.
        match self
            .config
            .cors
            .as_ref()
            .filter(|cors| cors.is_configured())
        {
            Some(cors) => match cors_layer(cors) {
                Ok(layer) => {
                    tracing::debug!(
                        origins = ?cors.allowed_origins,
                        "cors enabled"
                    );
                    router.layer(layer)
                }
                Err(error) => {
                    // The router is already built; there is nowhere to report this but
                    // the log. `validate` catches it before the port is bound.
                    tracing::error!(error = %error, "cors configuration is invalid");
                    router
                }
            },
            None => router,
        }
    }

    /// Binds the port and serves until the context is cancelled.
    ///
    /// The accept loop lives in [`listener`](super::listener); this is the entry point
    /// that builds the router and hands over the cancellation token.
    ///
    /// # Errors
    ///
    /// Fails when the port cannot be bound.
    pub async fn serve(self: &Arc<Self>, context: Context) -> Result<()> {
        let tls = match self.config.ssl.as_ref().filter(|ssl| ssl.enabled) {
            Some(ssl) => Some(super::tls::CertStore::load(ssl)?),
            None => None,
        };

        // A certificate that changes on disk takes effect without a restart.
        let watcher = tls
            .as_ref()
            .and_then(|store| store.watch_files(context.cancellation_token()));

        let router = self.router(context.clone());
        let outcome =
            super::listener::serve(router, &self.config, context.cancellation_token(), tls).await;

        if let Some(watcher) = watcher {
            watcher.abort();
        }
        outcome
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
    let started = std::time::Instant::now();
    let (mut parts, body) = req.into_parts();

    let params = match Path::<HashMap<String, String>>::from_request_parts(&mut parts, &()).await {
        Ok(Path(params)) => params,
        Err(_) => HashMap::new(),
    };
    let upgrade = axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .ok();

    // A multipart body is left on the wire: uploads can be far larger than it is
    // sensible to hold in memory, and the parser can spill them to disk instead.
    let body = if crate::web::request::is_multipart_content_type(&parts) {
        Body::Streaming(Mutex::new(Some(body)))
    } else {
        let read = crate::web::request::read_to_bytes(body, state.config.effective_max_body_size());
        match crate::web::request::within_read_timeout(&state.config, read).await {
            Ok(bytes) => Body::Buffered(bytes),
            Err(error) => {
                let mut response = state
                    .converter
                    .convert(Err(error), &Request::for_test("/"))
                    .await;
                // A body that never finished arriving leaves the connection mid-request:
                // whatever the client sends next would be read as the tail of this one.
                // A 408 is the one answer that has to close.
                if response.status() == axum::http::StatusCode::REQUEST_TIMEOUT {
                    response.headers_mut().insert(
                        axum::http::header::CONNECTION,
                        axum::http::HeaderValue::from_static("close"),
                    );
                }
                return response;
            }
        }
    };

    let request = Request::new(
        parts,
        body,
        params,
        state.route.path().to_string(),
        Arc::clone(state.route.shared_meta()),
        state.config.clone(),
        state.context.clone(),
        upgrade,
    );

    let mut chain = FilterChain::new(state.filters.clone(), state.route.handler().clone());
    let result = chain.next(request.clone()).await;
    let response = state.converter.convert(result, &request).await;
    log_access(&request, state.route.path(), &response, started);
    response
}

/// One line per request.
///
/// Logged at `debug`, matching the Go original: raise `log.level` to `debug` (or narrow
/// it to `rust_web_frame=debug`) to switch access logging on. Failures are logged at
/// `error` by the converter as well, so they are visible without it.
fn log_access(
    request: &Request,
    route: &str,
    response: &axum::response::Response,
    started: std::time::Instant,
) {
    tracing::debug!(
        method = %request.method(),
        route,
        path = request.path(),
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        client = %request.client_ip(),
        "request"
    );
}

#[derive(Clone)]
struct FallbackState {
    config: Arc<ServerConfig>,
    converter: Arc<dyn Converter>,
    context: Context,
}

/// Routes requests that matched no route: static files, the SPA fallback, then 404.
async fn fallback(state: FallbackState, req: AxumRequest) -> axum::response::Response {
    let started = std::time::Instant::now();
    // The fallback bypasses the filter chain, so build a request by hand.
    let (parts, body) = req.into_parts();
    let body = crate::web::request::read_to_bytes(body, state.config.effective_max_body_size())
        .await
        .unwrap_or_default();
    let request = Request::new(
        parts.clone(),
        Body::Buffered(body),
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
        if let Ok(response) = super::routes::serve_file(&request, directory, &path).await {
            return state.converter.convert(Ok(response), &request).await;
        }
    }

    // 2. The SPA fallback page, for browser navigations only.
    if accepts_html(&request)
        && let Some(page) = &state.config.not_found_page
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
    let response = state.converter.convert(Err(error), &request).await;
    tracing::debug!(
        method = %request.method(),
        path = parts.uri.path(),
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

fn accepts_html(req: &Request) -> bool {
    req.header("accept")
        .is_some_and(|accept| accept.contains("text/html"))
}

/// Builds the CORS layer, rejecting anything that is not a valid header or method so
/// the mistake is a startup error rather than a request that quietly loses a header.
fn cors_layer(config: &super::config::CorsConfig) -> Result<CorsLayer> {
    let mut layer = CorsLayer::new();

    layer = if config.allowed_origins.iter().any(|origin| origin == "*") {
        layer.allow_origin(Any)
    } else {
        let mut origins = Vec::with_capacity(config.allowed_origins.len());
        for origin in &config.allowed_origins {
            origins.push(origin.parse::<axum::http::HeaderValue>().map_err(|err| {
                Error::with_source(500, err).detail(format!("`{origin}` is not a valid origin"))
            })?);
        }
        layer.allow_origin(origins)
    };

    layer = if config.allowed_headers.iter().any(|header| header == "*") {
        layer.allow_headers(Any)
    } else if config.allowed_headers.is_empty() {
        layer
    } else {
        let mut headers = Vec::with_capacity(config.allowed_headers.len());
        for header in &config.allowed_headers {
            headers.push(header.parse::<axum::http::HeaderName>().map_err(|err| {
                Error::with_source(500, err)
                    .detail(format!("`{header}` is not a valid request header name"))
            })?);
        }
        layer.allow_headers(headers)
    };

    if !config.allowed_methods.is_empty() {
        let mut methods = Vec::with_capacity(config.allowed_methods.len());
        for method in &config.allowed_methods {
            methods.push(
                Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|err| {
                    Error::with_source(500, err)
                        .detail(format!("`{method}` is not a valid HTTP method"))
                })?,
            );
        }
        layer = layer.allow_methods(methods);
    }

    if !config.exposed_headers.is_empty() {
        let mut headers = Vec::with_capacity(config.exposed_headers.len());
        for header in &config.exposed_headers {
            headers.push(header.parse::<axum::http::HeaderName>().map_err(|err| {
                Error::with_source(500, err)
                    .detail(format!("`{header}` is not a valid response header name"))
            })?);
        }
        layer = layer.expose_headers(headers);
    }

    if config.allow_credentials {
        layer = layer.allow_credentials(true);
    }
    if let Some(max_age) = config.max_age {
        layer = layer.max_age(std::time::Duration::from_secs(max_age));
    }

    Ok(layer)
}

/// Renders an error envelope with the given status.
///
/// Used where the converter cannot reach: the panic handler, which runs outside the
/// filter chain, and the method-not-allowed fallback, which never builds a request.
fn error_envelope_response(status: u16, code: i32, message: &str) -> axum::response::Response {
    let envelope = Message {
        code,
        data: serde_json::Value::Null,
        message: message.to_owned(),
        kind: String::new(),
    };
    let body = serde_json::to_string(&envelope).unwrap_or_else(|_| String::from("{}"));
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Turns a panicking handler into the framework's 500 envelope.
///
/// Without this a panic unwinds into hyper's connection task. The client then sees a
/// connection reset or a bare `503` with no body — indistinguishable from a real
/// capacity failure — and the only trace is the default panic hook, which does not go
/// through `tracing` and so never reaches the configured log file.
#[derive(Clone, Copy, Debug, Default)]
struct PanicHandler;

impl ResponseForPanic for PanicHandler {
    type ResponseBody = axum::body::Body;

    fn response_for_panic(
        &mut self,
        payload: Box<dyn std::any::Any + Send + 'static>,
    ) -> axum::response::Response {
        // The panic message is for the operator, not for the client.
        // `payload.as_ref()`, not `&payload`: `Box<dyn Any>` implements `Any` itself,
        // so a `&Box<...>` would be downcast against the box rather than its contents.
        tracing::error!(panic = %panic_message(payload.as_ref()), "a handler panicked");
        error_envelope_response(500, 500, "internal server error")
    }
}

/// Best-effort rendering of a panic payload, which is a `&str` or a `String` for every
/// panic raised by `panic!`.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    String::from("a handler panicked with a payload that is not a string")
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
    let mut response = error_envelope_response(405, 405, "method not allowed");
    if let Ok(value) = axum::http::HeaderValue::from_str(&allowed) {
        response.headers_mut().insert("allow", value);
    }
    response
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
