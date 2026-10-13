//! Reverse proxy: forwarding a path to another service.
//!
//! One HTTP client — connections pooled, TLS included — shared by every route that
//! forwards, and one handler per route. The handler goes wherever the application
//! registers it, so the route still carries metadata, still passes through the filter chain,
//! and still shows up in the access log under its own template.
//!
//! ```no_run
//! use async_trait::async_trait;
//! use rust_web_frame::component::Proxy;
//! use rust_web_frame::core::{Context, Service};
//! use rust_web_frame::web::Result;
//!
//! #[derive(Default)]
//! struct Gateway;
//!
//! #[async_trait]
//! impl Service for Gateway {
//!     async fn init(&self, ctx: &Context) -> Result<()> {
//!         let proxy = ctx.service::<Proxy>()?;
//!         proxy.register(ctx, "/api", "https://backend.internal:8443/v1")?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! With that, `GET /api/users/7?full=1` becomes
//! `GET https://backend.internal:8443/v1/users/7?full=1`. What travels: the method, the
//! path, the query, the body, and every header except the hop-by-hop ones — plus
//! `x-forwarded-for`, `x-forwarded-host` and `x-forwarded-proto`, so the upstream can still
//! see who asked and how. The response comes back the same way, streamed rather than
//! buffered: only the request body is held in memory, because the framework has already
//! read it by the time a handler runs.
//!
//! ```yaml
//! proxy:
//!   timeout: 30      # seconds to wait for the upstream's response head
//! ```
//!
//! # What is not proxied
//!
//! WebSocket upgrades: the server hands the upgrade to the handler that asked for it, and a
//! forwarded connection would have to be tunnelled rather than passed along. `https`
//! upstreams are trusted through the roots `webpki` ships, so a private authority needs its
//! certificate added to that store, not to the operating system's.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, HOST, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Request as HttpRequest, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};

use crate::core::{Context, Service};
use crate::web::{Error, Handler, Request, Response, Result, handler};

/// Config section that holds the proxy settings.
pub const PROXY_CONFIG_KEY: &str = "proxy";

/// Proxy configuration, read from the `proxy` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxyConfig {
    /// How long to wait for the upstream's response head, in seconds.
    ///
    /// The head, not the whole body: a response that arrives and then streams for a minute
    /// is a stream, and cutting it off would be the timeouts of whoever is reading it.
    pub timeout: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self { timeout: 30 }
    }
}

type Body = axum::body::Body;
type ProxyClient = Client<HttpsConnector<HttpConnector>, Body>;

/// The proxy: one connection pool, and a handler for every path it forwards.
pub struct Proxy {
    client: ProxyClient,
    /// Read from the `proxy` section at startup; behind a lock because
    /// [`init`](Service::init) takes `&self`.
    config: RwLock<ProxyConfig>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("timeout", &self.config().timeout)
            .finish()
    }
}

impl Default for Proxy {
    fn default() -> Self {
        Self::new(ProxyConfig::default())
    }
}

impl Proxy {
    /// A proxy with the given settings.
    #[must_use]
    pub fn new(config: ProxyConfig) -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            config: RwLock::new(config),
        }
    }

    /// A handler that forwards everything under `prefix` to `target`.
    ///
    /// The path below `prefix` is appended to whatever path `target` names, and the query
    /// string travels untouched. `tracing` gets one line per forward, at `debug`, naming
    /// where the request went.
    ///
    /// # Errors
    ///
    /// Fails when `target` is not an `http://` or `https://` url with a host. That is a
    /// mistake in the application rather than a runtime condition, so it is caught here, at
    /// startup, instead of on the first request.
    pub fn forward(&self, prefix: impl Into<String>, target: impl Into<String>) -> Result<Handler> {
        let prefix = prefix.into();
        let target = Target::parse(&target.into())?;
        let client = self.client.clone();
        let timeout = Duration::from_secs(self.config().timeout);

        Ok(handler(move |req: Request| {
            let client = client.clone();
            let prefix = prefix.clone();
            let target = target.clone();
            async move { forward(&client, &prefix, &target, timeout, &req).await }
        }))
    }

    /// Registers the two routes that cover `prefix`: the bare path, and everything below it.
    ///
    /// Both are needed. A `/*rest` route matches only a *non-empty* remainder — that is
    /// matchit's rule, not this component's — so a client asking for `/api` exactly would
    /// otherwise get the framework's `404` instead of the service's front page. A trailing
    /// slash (`/api/`) is matched by neither, the same way it is not matched by
    /// [`static_files`](crate::web::Routes::static_files).
    ///
    /// ```no_run
    /// # use rust_web_frame::component::Proxy;
    /// # use rust_web_frame::core::Context;
    /// # use rust_web_frame::web::Result;
    /// # fn routes(ctx: &Context) -> Result<()> {
    /// let proxy = ctx.service::<Proxy>()?;
    /// proxy.register(ctx, "/api", "https://backend.internal:8443/v1")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails the same way [`forward`](Self::forward) does, before anything is registered.
    pub fn register(&self, ctx: &Context, prefix: &str, target: impl Into<String>) -> Result<()> {
        let prefix = prefix.trim_end_matches('/');
        let handler = self.forward(prefix, target)?;
        // `register(ctx, "/", …)` forwards the whole site, which is a prefix of nothing in
        // particular and so a route of its own.
        let (bare, below) = if prefix.is_empty() {
            ("/", String::from("/*rest"))
        } else {
            (prefix, format!("{prefix}/*rest"))
        };

        ctx.any(bare, Arc::clone(&handler));
        ctx.any(&below, handler);
        Ok(())
    }

    /// The settings in force.
    #[must_use]
    pub fn config(&self) -> ProxyConfig {
        self.config.read().expect("proxy poisoned").clone()
    }
}

#[async_trait]
impl Service for Proxy {
    /// Reads the `proxy` section into this instance.
    ///
    /// The registered component is the one the container keeps, so the settings are applied
    /// to it rather than to a second copy — a component that replaced itself here would
    /// leave `ctx.service::<Proxy>()` and the client doing the forwarding as two different
    /// objects.
    async fn init(&self, ctx: &Context) -> Result<()> {
        let config: ProxyConfig = ctx.config().section(PROXY_CONFIG_KEY).unwrap_or_default();

        tracing::debug!(timeout = config.timeout, "proxy ready");
        *self.config.write().expect("proxy poisoned") = config;
        Ok(())
    }
}

/// Where a forwarded request goes: a scheme, an authority, and the path it lands on.
#[derive(Debug, Clone)]
struct Target {
    scheme: &'static str,
    /// `host` or `host:port`, as written.
    authority: String,
    /// `""` or a path without a trailing slash, e.g. `/v1`.
    base_path: String,
}

impl Target {
    /// Parses `scheme://host[:port][/base]`.
    fn parse(url: &str) -> Result<Self> {
        let refused = || {
            Error::new(500, format!("`{url}` is not a usable proxy target"))
                .detail("a target is `http://host[:port][/base]` or `https://host[:port][/base]`")
        };

        let (scheme, rest) = url.split_once("://").ok_or_else(refused)?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => "http",
            "https" => "https",
            _ => return Err(refused()),
        };
        let (authority, base_path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{}", path.trim_end_matches('/'))),
            None => (rest, String::new()),
        };
        if authority.is_empty() {
            return Err(refused());
        }

        Ok(Self {
            scheme,
            authority: authority.to_owned(),
            base_path,
        })
    }

    /// The upstream uri for a request whose own path starts with `prefix`.
    fn resolve(&self, prefix: &str, uri: &Uri) -> String {
        let path = uri.path();
        let remainder = path.strip_prefix(prefix).unwrap_or(path);
        let remainder = remainder.trim_start_matches('/');
        let path = if remainder.is_empty() {
            // `https://host` with an empty path is not a request line; `/` is.
            if self.base_path.is_empty() {
                String::from("/")
            } else {
                self.base_path.clone()
            }
        } else {
            format!("{}/{remainder}", self.base_path)
        };

        match uri.query() {
            Some(query) => format!("{}://{}{}?{query}", self.scheme, self.authority, path),
            None => format!("{}://{}{path}", self.scheme, self.authority),
        }
    }
}

/// Forwards one request upstream, and hands back what came back.
async fn forward(
    client: &ProxyClient,
    prefix: &str,
    target: &Target,
    timeout: Duration,
    req: &Request,
) -> Result<Response> {
    let location = target.resolve(prefix, req.uri());
    let uri: Uri = location.parse().map_err(|_| {
        Error::new(502, "the request cannot be forwarded")
            .detail(format!("`{location}` is not a uri"))
    })?;

    let mut builder = HttpRequest::builder().method(req.method().clone()).uri(uri);
    if let Some(headers) = builder.headers_mut() {
        forward_headers(req, headers, target);
    }
    // The framework has already read the body — bounded by `maxBodySize` — by the time a
    // handler runs, so it travels as it stands rather than as a stream.
    let body = Body::from(req.read_body().await?.clone());
    let outgoing = builder
        .body(body)
        .map_err(|err| Error::with_source(502, err).detail("the forwarded request is malformed"))?;

    let response = tokio::time::timeout(timeout, client.request(outgoing))
        .await
        .map_err(|_| {
            Error::new(504, "the upstream service took too long").detail(format!(
                "{} did not answer within {}s",
                target.authority,
                timeout.as_secs()
            ))
        })?
        .map_err(|err| {
            Error::with_source(502, err).detail(format!("cannot reach {}", target.authority))
        })?;

    tracing::debug!(upstream = %location, status = %response.status(), "forwarded");

    let (mut parts, upstream) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Ok(Response::raw(axum::response::Response::from_parts(
        parts,
        Body::new(upstream),
    )))
}

/// Copies the request headers upstream.
///
/// Everything but the hop-by-hop headers and the two that only make sense between the
/// client and us: `host`, which names the upstream instead, and `content-length`, which
/// hyper recomputes from the body it is actually given.
fn forward_headers(req: &Request, headers: &mut HeaderMap, target: &Target) {
    let hop_by_hop = connection_tokens(req.headers());

    for (name, value) in req.headers() {
        if is_hop_by_hop(name) || hop_by_hop.contains(name) {
            continue;
        }
        if name == HOST || name == CONTENT_LENGTH {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    if let Ok(value) = HeaderValue::from_str(&target.authority) {
        headers.insert(HOST, value);
    }
    // What every proxy tells its upstream, so it can log the real caller and build absolute
    // urls that point back at us rather than at itself.
    if let Ok(value) = HeaderValue::from_str(&req.client_ip()) {
        headers.insert("x-forwarded-for", value);
    }
    if let Ok(value) = HeaderValue::from_str(req.host()) {
        headers.insert("x-forwarded-host", value);
    }
    let secure = req
        .server_config()
        .ssl
        .as_ref()
        .is_some_and(|ssl| ssl.enabled);
    headers.insert(
        "x-forwarded-proto",
        HeaderValue::from_static(if secure { "https" } else { "http" }),
    );
}

/// Drops the headers that belong to one hop rather than to the message.
///
/// RFC 9110 §7.6.2: a proxy may relay what they mean, never the headers themselves — and
/// the ones a `Connection` header names are hop-by-hop for this message too. Copying any of
/// them is how `keep-alive` and `transfer-encoding` end up describing a connection the
/// header did not come from.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named = connection_tokens(headers);
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

/// The header names this message's `Connection` header declares hop-by-hop.
fn connection_tokens(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect()
}

/// The headers that are always hop-by-hop, whatever `Connection` says.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::Builder;
    use crate::config::Config;

    /// A backend that reports back what it was sent, which is how the tests look at the
    /// forwarded request from the other side.
    async fn backend() -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().fallback(axum::routing::any(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), server)
    }

    async fn echo(request: axum::extract::Request) -> axum::response::Response {
        let (parts, body) = request.into_parts();
        let received = axum::body::to_bytes(body, 1 << 20)
            .await
            .unwrap_or_default();
        let headers: Value = parts
            .headers
            .iter()
            .map(|(name, value)| (name.to_string(), json!(value.to_str().unwrap_or_default())))
            .collect::<serde_json::Map<_, _>>()
            .into();

        axum::response::Response::builder()
            .status(201)
            .header("x-echo", "the backend")
            .body(axum::body::Body::from(
                serde_json::to_vec(&json!({
                    "method": parts.method.to_string(),
                    "uri": parts.uri.to_string(),
                    "body": String::from_utf8_lossy(&received),
                    "headers": headers,
                }))
                .unwrap(),
            ))
            .unwrap()
    }

    /// An application that forwards `/api` to `target`.
    async fn app(target: &str) -> axum::Router {
        let target = target.to_owned();
        let mut builder = Builder::new(Config::empty());
        builder.service(Proxy::default());
        builder.routes(move |ctx| {
            let proxy = ctx.service::<Proxy>().expect("registered above");
            proxy
                .register(ctx, "/api", target.clone())
                .expect("a usable target");
        });
        builder
            .build()
            .into_router()
            .await
            .expect("the application initialises")
    }

    /// Sends `request` and returns (status, headers, body).
    async fn send(router: &axum::Router, request: HttpRequest<Body>) -> (u16, HeaderMap, Vec<u8>) {
        let response = router.clone().oneshot(request).await.expect("served");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("readable")
            .to_vec();
        (status, headers, body)
    }

    fn get(path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_request_reaches_the_upstream_and_its_answer_comes_back() {
        let (target, _server) = backend().await;
        let router = app(&format!("{target}/v1")).await;

        let (status, headers, body) = send(&router, get("/api/users/7?full=1")).await;
        let echoed: Value = serde_json::from_slice(&body).unwrap();

        // The upstream's answer, status and headers included.
        assert_eq!(status, 201);
        assert_eq!(headers["x-echo"], "the backend");
        // And the request it saw: the path below the prefix, appended to the target's.
        assert_eq!(echoed["uri"], "/v1/users/7?full=1");
        assert_eq!(echoed["method"], "GET");
    }

    #[tokio::test]
    async fn the_method_body_and_host_travel_with_it() {
        let (target, _server) = backend().await;
        let router = app(&target).await;

        let request = HttpRequest::builder()
            .method("POST")
            .uri("/api/orders")
            .header("content-type", "application/json")
            .header("host", "gateway.example.com")
            .body(Body::from(r#"{"id":7}"#))
            .unwrap();
        let (_, _, body) = send(&router, request).await;
        let echoed: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(echoed["method"], "POST");
        assert_eq!(echoed["body"], r#"{"id":7}"#);
        assert_eq!(echoed["headers"]["content-type"], "application/json");
        // The upstream is asked for *its* host, not the one the client used.
        let authority = target.trim_start_matches("http://");
        assert_eq!(echoed["headers"]["host"], authority);
        assert_eq!(echoed["headers"]["x-forwarded-host"], "gateway.example.com");
        assert_eq!(echoed["headers"]["x-forwarded-proto"], "http");
        assert!(echoed["headers"]["x-forwarded-for"].is_string());
    }

    #[tokio::test]
    async fn the_bare_prefix_lands_on_the_target_too() {
        let (target, _server) = backend().await;
        // A trailing slash on the target, and none left over on the way upstream.
        let router = app(&format!("{target}/v1/")).await;

        // `/api` exactly, which is the route `register` adds beside the catch-all: matchit
        // matches a `/*rest` route only when something follows the slash.
        let (status, _, body) = send(&router, get("/api")).await;
        assert_eq!(status, 201);
        let echoed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(echoed["uri"], "/v1");
    }

    #[tokio::test]
    async fn hop_by_hop_headers_do_not_travel() {
        let (target, _server) = backend().await;
        let router = app(&target).await;

        let request = HttpRequest::builder()
            .uri("/api/thing")
            .header("connection", "keep-alive, x-drop-me")
            .header("x-drop-me", "secret")
            .header("keep-alive", "timeout=5")
            .header("x-keep-me", "yes")
            .body(Body::empty())
            .unwrap();
        let (_, _, body) = send(&router, request).await;
        let echoed: Value = serde_json::from_slice(&body).unwrap();

        assert!(echoed["headers"]["x-drop-me"].is_null(), "{echoed}");
        assert!(echoed["headers"]["keep-alive"].is_null(), "{echoed}");
        assert!(echoed["headers"]["connection"].is_null(), "{echoed}");
        assert_eq!(echoed["headers"]["x-keep-me"], "yes");
    }

    #[tokio::test]
    async fn an_upstream_that_is_not_there_is_a_502() {
        // Bound and dropped, so the port is nobody's.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let router = app(&format!("http://{address}")).await;
        let (status, _, body) = send(&router, get("/api/thing")).await;

        assert_eq!(status, 502, "{}", String::from_utf8_lossy(&body));
    }

    #[tokio::test]
    async fn an_upstream_that_never_answers_is_a_504() {
        // Accepts the connection and then says nothing at all.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let silence = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });

        let target = format!("http://{address}");
        let mut builder = Builder::new(Config::empty());
        builder.service(Proxy::new(ProxyConfig { timeout: 1 }));
        builder.routes(move |ctx| {
            let proxy = ctx.service::<Proxy>().expect("registered above");
            proxy
                .register(ctx, "/api", target.clone())
                .expect("a usable target");
        });
        let router = builder.build().into_router().await.expect("initialises");

        let (status, _, _) = send(&router, get("/api/thing")).await;
        assert_eq!(status, 504);

        silence.abort();
    }

    #[test]
    fn a_target_that_is_not_a_url_is_refused_at_startup() {
        let proxy = Proxy::default();
        for bad in [
            "backend.internal",
            "ftp://backend.internal",
            "https://",
            "://backend.internal",
        ] {
            assert!(proxy.forward("/api", bad).is_err(), "{bad} was accepted");
        }

        assert!(proxy.forward("/api", "http://127.0.0.1:8080").is_ok());
        assert!(proxy.forward("/api", "https://backend.internal/v1").is_ok());
    }
}
