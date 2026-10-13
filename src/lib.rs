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
//! **2. Queries stay sea-orm's.** The framework binds the entity to a connection, derives
//! the table from it, and puts it in the container — it does not add a query language on
//! top. `filter(Column::Age.gte(18))` is checked by the compiler at the call site, and the
//! sea-orm documentation applies as written:
//!
//! ```no_run
//! use rust_web_frame::define_model;
//! use rust_web_frame::model::{Page, Paged};
//! # use sea_orm::entity::prelude::*;
//! # pub mod user {
//! #     use sea_orm::entity::prelude::*;
//! #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
//! #     #[sea_orm(table_name = "t_user")]
//! #     pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String, pub age: i32 }
//! #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
//! #     impl ActiveModelBehavior for ActiveModel {}
//! # }
//! define_model!(UserModel, user::Entity);
//!
//! use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
//!
//! async fn demo(users: &UserModel, page: Page) -> Result<(), rust_web_frame::web::Error> {
//!     let one = user::Entity::find_by_id(1i64).one(users.db()).await?;
//!     let adults = user::Entity::find()
//!         .filter(user::Column::Age.gte(18))
//!         .order_by_desc(user::Column::Id)
//!         .all(users.db())
//!         .await?;
//!     let paged: Paged<user::Model> = users
//!         .page(user::Entity::find().filter(user::Column::Age.gte(18)), &page)
//!         .await?;
//!     # let _ = (one, adults, paged);
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

pub mod component;
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
    pub use crate::model::{Model, Page, Paged, TableEntity};
    pub use crate::web::{
        Error, Filter, IntoResponse, Message, MetaOption, Request, Response, Result, handler,
    };
    pub use crate::{Builder, WebFrame, define_model};
}
