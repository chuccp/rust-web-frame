//! # rust-web-frame
//!
//! An opinionated, batteries-included web framework for Rust, ported from the design of
//! [`go-web-frame`](https://github.com/chuccp/go-web-frame).
//!
//! The framework is built on top of [axum] (routing / HTTP), [sea-orm] (data access),
//! `tracing` (logging) and a small hand written configuration layer — the same way the Go
//! original is built on Gin + GORM + Zap + Viper.
//!
//! ## The three ideas
//!
//! **1. Declarative routing.** Policy belongs on the route, not in the handler. Tag a route
//! with [`RouteHandle::with_meta`](web::RouteHandle::with_meta) and one filter enforces it
//! for every route that opted in:
//!
//! ```no_run
//! use rust_web_frame::core::Context;
//! use rust_web_frame::web::{MetaOption, Request, Response, Result, handler};
//!
//! fn require_auth() -> MetaOption {
//!     MetaOption::value("require_auth", true)
//! }
//!
//! async fn profile(_req: Request) -> Result<Response> {
//!     Ok(Response::text("profile"))
//! }
//!
//! fn routes(ctx: &Context) -> Result<()> {
//!     ctx.get("/api/profile", handler(profile)).with_meta(require_auth());
//!     ctx.get("/api/login", handler(profile)); // public
//!     Ok(())
//! }
//! ```
//!
//! **2. A generic model that erases CRUD boilerplate.** Wrap a sea-orm entity in
//! [`model::Model`] and the compiler checks types from the database to the handler:
//!
//! ```no_run
//! use rust_web_frame::model::Page;
//! # use sea_orm::entity::prelude::*;
//! # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
//! # #[sea_orm(table_name = "t_user")]
//! # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
//! # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
//! # impl ActiveModelBehavior for ActiveModel {}
//! async fn demo(db: rust_web_frame::db::Database) -> Result<(), rust_web_frame::web::Error> {
//!     let users = rust_web_frame::model::Model::<Entity>::new(db, "t_user");
//!     let _one = users.find_by_pk(1).await?;
//!     let _adults = users
//!         .query()
//!         .filter("age > ?", rust_web_frame::args![18])
//!         .order_by_desc("id")
//!         .all()
//!         .await?;
//!     let (_list, _total) = users.page(&Page::new(1, 10)).await?;
//!     Ok(())
//! }
//! ```
//!
//! **3. One lifecycle.** [`Builder`] collects services, models, controllers, filters and
//! runners; `Build().run()` initialises them through a dependency-injection
//! [`Context`](core::Context) and
//! serves them.
//!
//! ```no_run
//! use rust_web_frame as wf;
//! use wf::config::load_auto_config;
//! use wf::web::handler;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let mut builder = wf::Builder::new(load_auto_config()?);
//! builder.get("/", handler(|_req| async { Ok("hello world") }));
//! builder.get("/api/profile", handler(|_req| async { Ok("profile") }));
//!
//! builder.build().run().await?;
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod core;
pub mod db;
pub mod log;
pub mod model;
pub mod util;
pub mod value;
pub mod web;

mod frame;

pub use frame::{Builder, WebFrame};

/// Convenience re-exports of the traits you need in scope to write an application.
///
/// ```
/// use rust_web_frame::prelude::*;
/// ```
pub mod prelude {
    pub use crate::core::{Context, Rest, Runner, Service};
    pub use crate::db::Database;
    pub use crate::model::{Model, Page, Paged, TableEntity, Transaction};
    pub use crate::web::{
        Error, Filter, IntoResponse, Message, MetaOption, Request, Response, Result, handler,
    };
    pub use crate::{Builder, WebFrame, define_model};
}
