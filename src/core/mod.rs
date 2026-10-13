//! The component model: services, the dependency-injection context, and the
//! application runtime.
//!
//! Everything in an application is a [`Service`]. Services are registered on a
//! [`Builder`](crate::Builder), initialised in order through a [`Context`], and then
//! either served ([`Rest`]), run in the background ([`Runner`]), or both.
//!
//! ```no_run
//! use rust_web_frame as wf;
//! use wf::core::Context;
//! use wf::web::{handler, Result};
//!
//! #[derive(Default)]
//! struct Users {
//!     greeting: std::sync::OnceLock<String>,
//! }
//!
//! #[async_trait::async_trait]
//! impl wf::core::Service for Users {
//!     async fn init(&self, ctx: &Context) -> Result<()> {
//!         let _ = self.greeting.set(ctx.config().get_or("app.greeting", "hello".into()));
//!         Ok(())
//!     }
//! }
//!
//! # async fn demo() -> Result<()> {
//! let mut builder = wf::Builder::new(wf::config::load_auto_config()?);
//! builder.service(Users::default());
//! let app = builder.build();
//! # let _ = app;
//! # Ok(())
//! # }
//! ```

mod context;
mod model_group;
mod registry;
mod rest_group;
mod runtime;
mod service;

pub use context::Context;
pub use model_group::{DEFAULT_GROUP, ModelGroup, ModelGroupBuilder};
pub use rest_group::{REST_GROUPS_CONFIG_KEY, RestGroup, RestGroupBuilder};
pub use runtime::Runtime;
pub use service::{ConverterService, Rest, Runner, Service};

pub(crate) use model_group::RegisteredGroup;
pub(crate) use registry::{Registration, WiredComponents, init_all};
pub(crate) use rest_group::{default_server_config, ensure_distinct_ports};
