//! The HTTP layer — the "Gin" of this framework.
//!
//! * [`Request`] / [`Response`] / [`Message`] — what handlers see and return.
//! * [`handler`] — turns an async closure or function into a route endpoint.
//! * [`Filter`] — middleware that reads per-route metadata through [`Request::has_meta`].
//! * [`MetaOption`] / [`RouteHandle::with_meta`] — the declarative routing hook.
//! * [`Server`] — binds a port, runs the chain, renders the result.
//!
//! # Per-route metadata
//!
//! ```no_run
//! use rust_web_frame::core::{Context, Rest};
//! use rust_web_frame::web::{handler, MetaOption, Request, Response, Result};
//!
//! fn require_auth() -> MetaOption {
//!     MetaOption::value("require_auth", true)
//! }
//!
//! #[derive(Default)]
//! struct Api;
//!
//! #[async_trait::async_trait]
//! impl Rest for Api {
//!     async fn init(&self, ctx: &Context) -> Result<()> {
//!         ctx.get("/api/profile", handler(profile)).with_meta(require_auth());
//!         ctx.get("/api/login", handler(profile)); // public
//!         Ok(())
//!     }
//! }
//!
//! async fn profile(_req: Request) -> Result<Response> {
//!     Ok(Response::text("profile"))
//! }
//! ```

pub mod config;
pub mod converter;
pub mod error;
pub mod file;
pub mod filter;
pub mod listener;
pub mod message;
pub mod multipart;
pub mod request;
pub mod response;
pub mod route;
pub mod routes;
pub mod server;
pub mod sse;
pub mod tls;
pub mod ws;

pub use config::{
    DEFAULT_MAX_BODY_SIZE, DEFAULT_PORT, SERVER_CONFIG_KEY, ServerConfig, SslCertificate, SslConfig,
};
pub use converter::{Converter, DefaultConverter, error_envelope};
pub use error::{Error, Result, code};
pub use file::FileResponse;
pub use filter::{BoxFuture, Filter, FilterChain, FnFilter, Handler, filter_fn, handler};
pub use message::Message;
pub use multipart::{
    DEFAULT_MAX_UPLOAD_MEMORY, MultipartForm, MultipartLimits, UploadedFile,
    parse as parse_multipart, sanitise_file_name,
};
pub use request::{Request, SetCookie};
pub use response::{IntoResponse, Response, ResponseBody};
pub use route::{HandlerMeta, MetaOption, Route, RouteHandle, RouteKind};
pub use routes::Routes;
pub use tls::CertStore;

/// Pagination types live with the model layer but are re-exported here, because a
/// handler reaches for them far more often than a model does.
pub use crate::util::{DEFAULT_PAGE_NO, DEFAULT_PAGE_SIZE, Page, Paged};
pub use server::Server;
pub use sse::{SseHandler, SseStream};
pub use ws::{Frame, MessageType, WebSocketHandler, WebSocketStream};
