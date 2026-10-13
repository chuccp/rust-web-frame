//! The request wrapper handed to handlers and filters.

use std::any::Any;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::ws::WebSocketUpgrade;
use axum::http::request::Parts;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::sync::OnceCell;

use super::config::ServerConfig;
use super::error::{Error, Result};
use super::multipart::{MultipartForm, MultipartLimits};
use super::route::{HandlerMeta, MetaOption};
use crate::core::Context;
use crate::util::Page;
use crate::util::percent_decode;

/// Returned when a request body is asked for twice, or by two different accessors.
const BODY_ALREADY_READ: &str = "the request body has already been consumed — a body can be read once, by either \
     `read_body()` or `multipart()`";

/// Where the request body lives.
///
/// Everything except a multipart upload arrives already buffered. An upload is left as a
/// stream, because it can be far larger than it is sensible to hold in memory — see
/// [`Request::multipart`].
pub(crate) enum Body {
    /// Read into memory before the handler ran.
    Buffered(Bytes),
    /// Still on the wire. Consumed by the first of `multipart()` / `read_body()`.
    Streaming(Mutex<Option<axum::body::Body>>),
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buffered(bytes) => f.debug_tuple("Buffered").field(&bytes.len()).finish(),
            Self::Streaming(_) => f.write_str("Streaming(..)"),
        }
    }
}

/// Everything a handler needs to know about the incoming request.
///
/// `Request` is a cheap handle: cloning it clones an `Arc`, so filters can pass it
/// downstream, stash a copy for post-processing, or move it into a spawned task.
#[derive(Clone)]
pub struct Request {
    inner: Arc<Inner>,
}

struct Inner {
    parts: Parts,
    body: Body,
    params: HashMap<String, String>,
    route_path: String,
    meta: Arc<HandlerMeta>,
    server_config: Arc<ServerConfig>,
    context: Context,
    ws_upgrade: Mutex<Option<WebSocketUpgrade>>,
    json: OnceLock<Result<Value, Error>>,
    form: OnceLock<HashMap<String, String>>,
    query: OnceLock<HashMap<String, String>>,
    buffered: OnceCell<Bytes>,
    multipart: OnceCell<MultipartForm>,
    attrs: Mutex<HashMap<String, Arc<dyn Any + Send + Sync>>>,
    cookies_to_set: Mutex<Vec<SetCookie>>,
    response_headers: Mutex<Vec<(String, String)>>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method().as_str())
            .field("path", &self.path())
            .field("route", &self.inner.route_path)
            .finish_non_exhaustive()
    }
}

/// An outgoing `Set-Cookie` header queued by [`Request::set_cookie`].
#[derive(Debug, Clone)]
pub struct SetCookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// `Max-Age` in seconds; `0` clears the cookie.
    pub max_age: i64,
    /// `Path` attribute.
    pub path: String,
    /// Whether to add `HttpOnly`.
    pub http_only: bool,
}

impl SetCookie {
    /// Renders the header value.
    pub fn to_header_value(&self) -> String {
        let mut out = format!("{}={}; Path={}", self.name, self.value, self.path);
        if self.max_age == 0 {
            out.push_str("; Max-Age=0");
        } else {
            out.push_str(&format!("; Max-Age={}", self.max_age));
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        out
    }
}

impl Request {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        parts: Parts,
        body: Body,
        params: HashMap<String, String>,
        route_path: String,
        meta: Arc<HandlerMeta>,
        server_config: Arc<ServerConfig>,
        context: Context,
        ws_upgrade: Option<WebSocketUpgrade>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                parts,
                body,
                params,
                route_path,
                meta,
                server_config,
                context,
                ws_upgrade: Mutex::new(ws_upgrade),
                json: OnceLock::new(),
                form: OnceLock::new(),
                query: OnceLock::new(),
                buffered: OnceCell::new(),
                multipart: OnceCell::new(),
                attrs: Mutex::new(HashMap::new()),
                cookies_to_set: Mutex::new(Vec::new()),
                response_headers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Builds a bare request for unit tests.
    pub fn for_test(path: &str) -> Self {
        use axum::http::Request as HttpRequest;
        let (parts, _) = HttpRequest::builder()
            .uri(path)
            .body(())
            .expect("valid test uri")
            .into_parts();
        Self::new(
            parts,
            Body::Buffered(Bytes::new()),
            HashMap::new(),
            path.to_string(),
            Arc::new(HandlerMeta::new()),
            Arc::new(ServerConfig::default()),
            Context::empty(),
            None,
        )
    }

    // ---------------------------------------------------------------- basics

    /// The HTTP method.
    pub fn method(&self) -> &axum::http::Method {
        &self.inner.parts.method
    }

    /// The request path, with the context path still attached.
    pub fn path(&self) -> &str {
        self.inner.parts.uri.path()
    }

    /// The matched route template, e.g. `/users/:id`.
    pub fn full_path(&self) -> &str {
        &self.inner.route_path
    }

    /// The raw query string, if any.
    pub fn query_string(&self) -> Option<&str> {
        self.inner.parts.uri.query()
    }

    /// The parsed request URI.
    pub fn uri(&self) -> &axum::http::Uri {
        &self.inner.parts.uri
    }

    /// The request headers.
    pub fn headers(&self) -> &axum::http::HeaderMap {
        &self.inner.parts.headers
    }

    /// Reads a header as a string (case insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner
            .parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
    }

    /// The `Content-Type` header.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Reports whether this is a `GET`.
    pub fn is_get(&self) -> bool {
        self.method() == axum::http::Method::GET
    }

    /// Reports whether this is a `POST`.
    pub fn is_post(&self) -> bool {
        self.method() == axum::http::Method::POST
    }

    /// Reports whether the body is a multipart form.
    pub fn is_multipart(&self) -> bool {
        self.content_type()
            .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
    }

    /// The configured context path (route prefix), e.g. `/api`.
    pub fn context_path(&self) -> &str {
        &self.inner.server_config.context_path
    }

    /// The server configuration this request was served with.
    #[must_use]
    pub fn server_config(&self) -> &ServerConfig {
        &self.inner.server_config
    }

    /// The application context — the entry point for looking services up at request time.
    #[must_use]
    pub fn context(&self) -> &Context {
        &self.inner.context
    }

    // ------------------------------------------------------------ parameters

    /// A path parameter, e.g. the `id` in `/users/:id`.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.inner.params.get(key).map(String::as_str)
    }

    /// A path parameter, or `default` when absent.
    pub fn param_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.param(key).unwrap_or(default)
    }

    /// A path parameter parsed as `i64`; `0` when absent or unparsable.
    pub fn param_i64(&self, key: &str) -> i64 {
        parse_i64(self.param(key))
    }

    /// A path parameter parsed as `u64`; `0` when absent or unparsable.
    pub fn param_u64(&self, key: &str) -> u64 {
        parse_u64(self.param(key))
    }

    /// All path parameters.
    pub fn params(&self) -> &HashMap<String, String> {
        &self.inner.params
    }

    /// A query string parameter.
    pub fn query(&self, key: &str) -> Option<&str> {
        self.query_map().get(key).map(String::as_str)
    }

    /// Every query parameter, percent-decoded. Parsed once and cached.
    #[must_use]
    pub fn query_map(&self) -> &HashMap<String, String> {
        self.inner
            .query
            .get_or_init(|| parse_pairs(self.query_string().unwrap_or("")))
    }

    /// A query string parameter, or `default` when absent.
    pub fn query_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.query(key).unwrap_or(default)
    }

    /// A query parameter parsed as `i64`; `0` when absent or unparsable.
    pub fn query_i64(&self, key: &str) -> i64 {
        parse_i64(self.query(key))
    }

    /// A query parameter parsed as `u64`; `0` when absent or unparsable.
    pub fn query_u64(&self, key: &str) -> u64 {
        parse_u64(self.query(key))
    }

    /// A query parameter parsed as `f64`; `0.0` when absent or unparsable.
    pub fn query_f64(&self, key: &str) -> f64 {
        self.query(key).and_then(|v| v.parse().ok()).unwrap_or(0.0)
    }

    /// A body/form parameter. Reads `application/x-www-form-urlencoded` bodies and, for
    /// other content types, falls back to the query string.
    /// A form parameter. Reads `application/x-www-form-urlencoded` bodies and, for
    /// other content types, falls back to the query string. Multipart fields are not
    /// included — reach for [`multipart`](Self::multipart) on those requests.
    pub fn form(&self, key: &str) -> Option<&str> {
        if let Some(value) = self.form_map().get(key) {
            return Some(value.as_str());
        }
        self.query(key)
    }

    fn form_map(&self) -> &HashMap<String, String> {
        self.inner.form.get_or_init(|| {
            let is_urlencoded = self.content_type().is_some_and(|ct| {
                ct.to_ascii_lowercase()
                    .starts_with("application/x-www-form-urlencoded")
            });
            if !is_urlencoded {
                return HashMap::new();
            }
            let Some(body) = self.body() else {
                return HashMap::new();
            };
            parse_pairs(std::str::from_utf8(body).unwrap_or(""))
        })
    }

    // ------------------------------------------------------------------ body

    /// The raw request body, when it was read into memory before the handler ran.
    ///
    /// `None` for a multipart upload, whose body is deliberately left on the wire — call
    /// [`multipart`](Self::multipart) for those, or [`read_body`](Self::read_body) to pull
    /// it into memory regardless.
    #[must_use]
    pub fn body(&self) -> Option<&Bytes> {
        match &self.inner.body {
            Body::Buffered(bytes) => Some(bytes),
            Body::Streaming(_) => None,
        }
    }

    /// Reads the whole body into memory, whatever its content type.
    ///
    /// The result is cached, so calling this twice is free.
    ///
    /// # Errors
    ///
    /// Fails with `413` when the body exceeds
    /// [`ServerConfig::max_body_size`](super::ServerConfig::max_body_size), or with `400`
    /// when the body was already consumed by [`multipart`](Self::multipart).
    pub async fn read_body(&self) -> Result<&Bytes> {
        match &self.inner.body {
            Body::Buffered(bytes) => Ok(bytes),
            Body::Streaming(slot) => {
                let limit = self.inner.server_config.effective_max_body_size();
                within_read_timeout(
                    &self.inner.server_config,
                    self.inner.buffered.get_or_try_init(|| async {
                        let body = slot
                            .lock()
                            .expect("body lock poisoned")
                            .take()
                            .ok_or_else(|| Error::bad_request().detail(BODY_ALREADY_READ))?;
                        read_to_bytes(body, limit).await
                    }),
                )
                .await
            }
        }
    }

    /// The body decoded as UTF-8, lossily.
    ///
    /// Returns `None` for a body that has not been read into memory — see
    /// [`body`](Self::body).
    #[must_use]
    pub fn body_string(&self) -> Option<String> {
        self.body()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// The parsed `multipart/form-data` body.
    ///
    /// The body is streamed through the parser: one file at a time is held in memory up
    /// to [`ServerConfig::max_upload_memory`](super::ServerConfig::max_upload_memory),
    /// and anything larger spills to a temporary file that is removed when the form is
    /// dropped. Memory use is therefore bounded per concurrent upload, not per upload
    /// size.
    ///
    /// The result is cached, and the body can only be consumed once.
    ///
    /// # Errors
    ///
    /// Fails with `400` when the request is not a well-formed multipart body or the body
    /// was already consumed, and with `413` when it exceeds the configured limits.
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{Error, Message, Request, Result};
    /// # use serde_json::json;
    /// async fn upload(req: Request) -> Result<Message> {
    ///     let form = req.multipart().await?;
    ///     let file = form
    ///         .file("avatar")
    ///         .ok_or_else(|| Error::bad_request().detail("avatar is required"))?;
    ///     Ok(Message::data(json!({
    ///         "name": file.sanitised_file_name(),
    ///         "size": file.size(),
    ///     })))
    /// }
    /// ```
    pub async fn multipart(&self) -> Result<&MultipartForm> {
        self.inner
            .multipart
            .get_or_try_init(|| async {
                let content_type = self.content_type().ok_or_else(|| {
                    Error::bad_request().detail("a multipart request needs a Content-Type header")
                })?;
                let limits = MultipartLimits {
                    max_body_size: self.max_body_size(),
                    max_upload_memory: self.inner.server_config.effective_max_upload_memory(),
                };

                match &self.inner.body {
                    Body::Streaming(slot) => {
                        let body = slot
                            .lock()
                            .expect("body lock poisoned")
                            .take()
                            .ok_or_else(|| Error::bad_request().detail(BODY_ALREADY_READ))?;
                        within_read_timeout(
                            &self.inner.server_config,
                            super::multipart::parse(body.into_data_stream(), content_type, limits),
                        )
                        .await
                    }
                    // A multipart body that arrived buffered — a test, or a caller that
                    // read it first — is parsed from memory.
                    Body::Buffered(bytes) => {
                        let stream = tokio_stream::once(Ok::<Bytes, std::io::Error>(bytes.clone()));
                        super::multipart::parse(stream, content_type, limits).await
                    }
                }
            })
            .await
    }

    /// Writes an uploaded file to `destination`, creating parent directories as needed.
    ///
    /// Returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Fails when the destination cannot be created or written.
    pub async fn save_uploaded_file(
        &self,
        file: &super::multipart::UploadedFile,
        destination: impl AsRef<std::path::Path>,
    ) -> Result<u64> {
        file.copy_to(destination).await
    }

    /// The multipart file-name limit used for a non-multipart body: the server's whole
    /// body limit.
    fn max_body_size(&self) -> u64 {
        match self.inner.server_config.effective_max_body_size() {
            usize::MAX => u64::MAX,
            other => other as u64,
        }
    }

    /// The body parsed as JSON. Parsed once and cached.
    ///
    /// `GET` requests are not allowed to carry JSON, whatever their content type.
    pub fn json(&self) -> Result<Value> {
        if self.is_get() {
            return Err(
                Error::bad_request().detail("JSON parameters are not supported in GET requests")
            );
        }
        let body = self.buffered_body()?;
        self.inner
            .json
            .get_or_init(|| {
                if body.is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_slice(body).map_err(Error::from)
            })
            .clone()
    }

    /// The body, provided it is already in memory.
    fn buffered_body(&self) -> Result<&Bytes> {
        self.body().ok_or_else(|| {
            Error::bad_request()
                .detail("this request carries a multipart upload — read it with `multipart()`")
        })
    }

    /// The body parsed as a JSON object.
    pub fn json_object(&self) -> Result<Map<String, Value>> {
        match self.json()? {
            Value::Object(map) => Ok(map),
            Value::Null => Ok(Map::new()),
            _ => Err(Error::bad_request().detail("invalid json object body")),
        }
    }

    /// Deserialises the JSON body into `T`.
    ///
    /// ```no_run
    /// # use rust_web_frame::web::Request;
    /// # use serde::Deserialize;
    /// #[derive(Deserialize)]
    /// struct CreateUser { name: String }
    ///
    /// async fn create(req: Request) -> Result<String, rust_web_frame::web::Error> {
    ///     let payload: CreateUser = req.bind_json()?;
    ///     Ok(payload.name)
    /// }
    /// ```
    pub fn bind_json<T: DeserializeOwned>(&self) -> Result<T> {
        let value = self.json()?;
        if value.is_null() {
            return Err(Error::bad_request().detail("request body is empty"));
        }
        serde_json::from_value(value).map_err(|err| {
            Error::bad_request()
                .detail(format!("invalid request payload: {err}"))
                .caused_by(err)
        })
    }

    /// A string field of the JSON body, or `default` when absent.
    pub fn json_str_or(&self, key: &str, default: &str) -> Result<String> {
        let object = self.json_object()?;
        Ok(object
            .get(key)
            .map(value_to_string)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_owned()))
    }

    /// An `i64` field of the JSON body, or `default` when absent.
    pub fn json_i64_or(&self, key: &str, default: i64) -> Result<i64> {
        let object = self.json_object()?;
        Ok(object.get(key).map(value_to_i64).unwrap_or(default))
    }

    /// An `f64` field of the JSON body, or `default` when absent.
    pub fn json_f64_or(&self, key: &str, default: f64) -> Result<f64> {
        let object = self.json_object()?;
        Ok(object.get(key).and_then(Value::as_f64).unwrap_or(default))
    }

    /// A `bool` field of the JSON body, or `default` when absent.
    pub fn json_bool_or(&self, key: &str, default: bool) -> Result<bool> {
        let object = self.json_object()?;
        Ok(object.get(key).and_then(Value::as_bool).unwrap_or(default))
    }

    // ------------------------------------------------------------ pagination

    /// Reads pagination parameters: from the query/form for `GET`, from the JSON body
    /// otherwise. Missing values fall back to `pageNo=1`, `pageSize=10`.
    pub fn page(&self) -> Result<Page> {
        // A multipart body carries files, not JSON — take the page from the query string.
        if self.is_get() || self.is_multipart() {
            return Ok(self.form_page());
        }
        let object = self.json_object()?;
        Ok(Page {
            page_no: object.get("pageNo").map(value_to_i64).unwrap_or(1),
            page_size: object.get("pageSize").map(value_to_i64).unwrap_or(10),
            last_id: object.get("lastId").map(value_to_i64).unwrap_or(0),
        })
    }

    /// Reads pagination parameters from the query string / form body only.
    pub fn form_page(&self) -> Page {
        Page {
            page_no: parse_i64(self.form("pageNo")),
            page_size: parse_i64(self.form("pageSize")),
            last_id: parse_i64(self.form("lastId")),
        }
    }

    // -------------------------------------------------------------- metadata

    /// The metadata attached to the matched route.
    #[must_use]
    pub fn handler_meta(&self) -> &HandlerMeta {
        &self.inner.meta
    }

    /// Reports whether the matched route satisfies **any** of `options`.
    ///
    /// This is how a filter asks "did this route opt in?":
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{MetaOption, Request};
    /// fn require_auth() -> MetaOption { MetaOption::value("require_auth", true) }
    /// fn check(req: &Request) -> bool { req.has_meta(require_auth()) }
    /// ```
    pub fn has_meta(&self, option: MetaOption) -> bool {
        option.matches(&self.inner.meta)
    }

    /// Reports whether the matched route satisfies any of several options.
    pub fn has_any_meta(&self, options: impl IntoIterator<Item = MetaOption>) -> bool {
        options.into_iter().any(|o| o.matches(&self.inner.meta))
    }

    // -------------------------------------------------------------- client

    /// The peer address, when the transport exposes one.
    pub fn remote_addr(&self) -> Option<&SocketAddr> {
        self.inner
            .parts
            .extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|c| &c.0)
    }

    /// The client IP, honouring `X-Forwarded-For` and `X-Real-IP`.
    pub fn client_ip(&self) -> String {
        if let Some(forwarded) = self.header("x-forwarded-for")
            && let Some(first) = forwarded.split(',').next()
        {
            let ip = first.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
        if let Some(real) = self.header("x-real-ip") {
            return real.trim().to_string();
        }
        self.remote_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_default()
    }

    /// The `Host` header, port included.
    pub fn host(&self) -> &str {
        self.header("host").unwrap_or("")
    }

    /// The `Host` header with any port stripped.
    pub fn domain(&self) -> &str {
        let host = self.host();
        match host.rfind(':') {
            Some(idx) => &host[..idx],
            None => host,
        }
    }

    // ------------------------------------------------------------- cookies

    /// Reads an incoming cookie.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let header = self.header("cookie")?;
        for pair in header.split(';') {
            let pair = pair.trim();
            if let Some((key, value)) = pair.split_once('=')
                && key.trim() == name
            {
                return Some(value.trim());
            }
        }
        None
    }

    /// Queues a `Set-Cookie` header on the response. `max_age` is in seconds; `0` clears
    /// the cookie.
    pub fn set_cookie(&self, name: impl Into<String>, value: impl Into<String>, max_age: i64) {
        self.set_cookie_with(name, value, max_age, "/", true);
    }

    /// Queues a `Set-Cookie` header with an explicit path and `HttpOnly` flag.
    pub fn set_cookie_with(
        &self,
        name: impl Into<String>,
        value: impl Into<String>,
        max_age: i64,
        path: impl Into<String>,
        http_only: bool,
    ) {
        self.inner
            .cookies_to_set
            .lock()
            .expect("cookies poisoned")
            .push(SetCookie {
                name: name.into(),
                value: value.into(),
                max_age,
                path: path.into(),
                http_only,
            });
    }

    /// Queues a `Set-Cookie` that removes the cookie.
    pub fn remove_cookie(&self, name: impl Into<String>) {
        self.set_cookie(name, "", 0);
    }

    pub(crate) fn take_cookies_to_set(&self) -> Vec<SetCookie> {
        std::mem::take(&mut *self.inner.cookies_to_set.lock().expect("cookies poisoned"))
    }

    /// Queues a response header, applied by the converter to **every** response.
    ///
    /// [`Response::set_header`](super::Response::set_header) only reaches the response a
    /// filter is holding — a handler that returns `Err` never gives it one. This is for the
    /// headers that have to survive that, `X-Request-Id` being the usual one:
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{Filter, FilterChain, Request, Response, Result};
    /// # struct Traced;
    /// # #[async_trait::async_trait]
    /// # impl Filter for Traced {
    /// # async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
    /// req.set_response_header("x-request-id", "3f2a…");
    /// chain.next(req).await
    /// # }
    /// # }
    /// ```
    pub fn set_response_header(&self, name: impl Into<String>, value: impl Into<String>) {
        self.inner
            .response_headers
            .lock()
            .expect("response headers poisoned")
            .push((name.into(), value.into()));
    }

    pub(crate) fn response_headers(&self) -> Vec<(String, String)> {
        self.inner
            .response_headers
            .lock()
            .expect("response headers poisoned")
            .clone()
    }

    // ---------------------------------------------------------- extensions

    /// Stores a value on the request, readable by downstream filters and the handler.
    ///
    /// This is how a filter hands data (a decoded token, a resolved user) to a handler.
    pub fn set_attr<T: Any + Send + Sync>(&self, key: impl Into<String>, value: T) {
        self.inner
            .attrs
            .lock()
            .expect("attrs poisoned")
            .insert(key.into(), Arc::new(value));
    }

    /// Reads a value stored with [`set_attr`](Self::set_attr).
    pub fn attr<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        self.inner
            .attrs
            .lock()
            .expect("attrs poisoned")
            .get(key)
            .and_then(|v| v.clone().downcast::<T>().ok())
    }

    // ------------------------------------------------------------ websocket

    /// Takes the pending WebSocket upgrade. Only a WebSocket route's converter calls
    /// this, and only once.
    pub fn take_websocket_upgrade(&self) -> Option<WebSocketUpgrade> {
        self.inner
            .ws_upgrade
            .lock()
            .expect("upgrade lock poisoned")
            .take()
    }

    /// The extension map of the underlying HTTP request.
    pub fn extensions(&self) -> &axum::http::Extensions {
        &self.inner.parts.extensions
    }
}

/// Runs `future` under the configured read timeout.
///
/// hyper bounds how long a client may take over its *headers*
/// ([`ServerConfig::read_header_timeout`]), but not over the body. This is the other
/// half, and it covers reading only — a slow *response* (an SSE stream, a WebSocket) is
/// never touched by it.
pub(crate) async fn within_read_timeout<T>(
    config: &ServerConfig,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match config.effective_read_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(408, "request timeout").detail(format!(
                "the request body did not arrive within {timeout:?} — raise `web.server.readTimeout` to allow slower clients"
            ))),
        },
        None => future.await,
    }
}

/// Reads a body into memory, failing with `413` once it exceeds `limit`.
///
/// The limit is enforced while reading rather than by `axum::body::to_bytes`, so the
/// error names the configuration key the operator has to change.
pub(crate) async fn read_to_bytes(body: axum::body::Body, limit: usize) -> Result<Bytes> {
    use futures_util::StreamExt;

    let mut stream = body.into_data_stream();
    let mut collected = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::from)?;
        if chunk.len() > limit.saturating_sub(collected.len()) {
            return Err(Error::new(413, "request body too large").detail(format!(
                "the body exceeds {limit} bytes — raise `web.server.maxBodySize` to accept it"
            )));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected.freeze())
}

/// Reports whether a request carries a `multipart/form-data` body.
pub(crate) fn is_multipart_content_type(parts: &Parts) -> bool {
    parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        })
}

fn parse_i64(value: Option<&str>) -> i64 {
    value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

fn parse_u64(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn value_to_i64(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Bool(b) => i64::from(*b),
        _ => 0,
    }
}

/// Parses `a=1&b=2` into a map, percent-decoding keys and values. The first occurrence
/// of a repeated key wins.
pub(crate) fn parse_pairs(input: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode(key);
        // The first occurrence of a repeated key wins.
        out.entry(key).or_insert_with(|| percent_decode(value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_pairs_are_percent_decoded() {
        let parsed = parse_pairs("name=hello%20world&city=%E5%8C%97%E4%BA%AC&flag");
        assert_eq!(parsed.get("name").unwrap(), "hello world");
        assert_eq!(parsed.get("city").unwrap(), "北京");
        assert_eq!(parsed.get("flag").unwrap(), "");
    }

    #[test]
    fn first_occurrence_of_a_repeated_key_wins() {
        let parsed = parse_pairs("a=1&a=2");
        assert_eq!(parsed.get("a").unwrap(), "1");
    }

    #[test]
    fn json_accessors_coerce_numbers_and_strings() {
        use axum::http::Request as HttpRequest;
        let (parts, _) = HttpRequest::builder()
            .method("POST")
            .uri("/x")
            .body(())
            .unwrap()
            .into_parts();
        let req = Request::new(
            parts,
            Body::Buffered(Bytes::from_static(
                br#"{"name":"alice","age":"31","admin":true}"#,
            )),
            HashMap::new(),
            "/x".to_string(),
            Arc::new(HandlerMeta::new()),
            Arc::new(ServerConfig::default()),
            Context::empty(),
            None,
        );
        assert_eq!(req.json_str_or("name", "").unwrap(), "alice");
        assert_eq!(req.json_i64_or("age", 0).unwrap(), 31);
        assert!(req.json_bool_or("admin", false).unwrap());
        assert_eq!(req.json_i64_or("missing", 7).unwrap(), 7);
    }

    #[test]
    fn get_requests_refuse_json_bodies() {
        let req = Request::for_test("/x");
        let err = req.json().unwrap_err();
        assert_eq!(err.code, 400);
    }

    #[test]
    fn page_defaults_match_the_go_original() {
        let req = Request::for_test("/x");
        let page = req.form_page().with_defaults();
        assert_eq!((page.page_no, page.page_size), (1, 10));
    }

    #[test]
    fn cookies_are_parsed_from_the_header() {
        use axum::http::Request as HttpRequest;
        let (parts, _) = HttpRequest::builder()
            .uri("/x")
            .header("cookie", "session=abc; theme=dark")
            .body(())
            .unwrap()
            .into_parts();
        let req = Request::new(
            parts,
            Body::Buffered(Bytes::new()),
            HashMap::new(),
            "/x".to_string(),
            Arc::new(HandlerMeta::new()),
            Arc::new(ServerConfig::default()),
            Context::empty(),
            None,
        );
        assert_eq!(req.cookie("session"), Some("abc"));
        assert_eq!(req.cookie("theme"), Some("dark"));
        assert_eq!(req.cookie("nope"), None);
    }

    #[test]
    fn attrs_round_trip_between_types() {
        let req = Request::for_test("/x");
        req.set_attr("user_id", 42u64);
        assert_eq!(req.attr::<u64>("user_id").map(|v| *v), Some(42));
        assert!(req.attr::<String>("user_id").is_none());
    }
}
