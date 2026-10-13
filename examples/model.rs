//! The generic model layer: one sea-orm entity, zero CRUD boilerplate.
//!
//! Runs on an in-memory SQLite database, so it needs no setup:
//!
//! ```text
//! cargo run --example model
//! curl http://localhost:19011/users
//! ```

use async_trait::async_trait;
use rust_web_frame as wf;
use rust_web_frame::args;
use rust_web_frame::config::load_auto_config;
use rust_web_frame::core::{Context, Rest};
use rust_web_frame::db::{DbConfig, connect};
use rust_web_frame::define_model;
use rust_web_frame::model::{Page, Transaction};
use rust_web_frame::web::{Message, Request, Response, Result, handler};
use sea_orm::Set;
use sea_orm::entity::prelude::*;
use serde_json::json;

// ---------------------------------------------------------------- the entity

pub mod user {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
    #[sea_orm(table_name = "t_user")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        #[sea_orm(unique)]
        pub email: String,
        pub name: String,
        pub age: i32,
        pub balance: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

// --------------------------------------------------------------- the model
//
// `define_model!` writes the newtype, a `Deref` to `EntryModel`, and the trait impls the
// framework needs. Everything `EntryModel` offers is then available on `UserModel`.

define_model!(
    /// Users of the shop.
    UserModel,
    user::Entity,
    "t_user"
);

impl UserModel {
    /// Business logic sits next to the queries it uses.
    pub async fn register(&self, email: &str, name: &str, age: i32) -> Result<user::Model> {
        self.insert(user::ActiveModel {
            email: Set(email.to_owned()),
            name: Set(name.to_owned()),
            age: Set(age),
            balance: Set(0),
            ..Default::default()
        })
        .await
    }

    /// Transfers money between two accounts atomically.
    pub async fn transfer(&self, from: i64, to: i64, amount: i64) -> Result<()> {
        Transaction::new(self.db().clone())
            .exec(|tx| async move {
                let accounts = UserModel::new(tx);
                let payer = accounts.get_by_pk(from).await?;
                if payer.balance < amount {
                    return Err(wf::web::Error::validation_failed().detail("insufficient funds"));
                }
                accounts
                    .update()
                    .where_pk(from)
                    .set("balance", payer.balance - amount)
                    .exec()
                    .await?;
                accounts
                    .update()
                    .where_pk(to)
                    .set("balance", payer.balance + amount)
                    .exec()
                    .await?;
                Ok(())
            })
            .await
    }
}

// ------------------------------------------------------------ the controller

#[derive(Default)]
struct UserController;

#[async_trait]
impl Rest for UserController {
    async fn init(&self, ctx: &Context) -> Result<()> {
        ctx.get("/users", handler(Self::list));
        ctx.get("/users/:id", handler(Self::show));
        ctx.get("/users/adults", handler(Self::adults));
        ctx.get("/users/stats", handler(Self::stats));
        ctx.post("/users/transfer", handler(Self::transfer));
        Ok(())
    }
}

impl UserController {
    fn model(req: &Request) -> std::sync::Arc<UserModel> {
        req.context()
            .service::<UserModel>()
            .expect("UserModel is registered before the controller")
    }

    async fn list(req: Request) -> Result<Message> {
        let page = req.page()?;
        Ok(Message::data(json!(
            Self::model(&req).page_for_web(&page).await?
        )))
    }

    async fn show(req: Request) -> Result<Message> {
        Ok(Message::data(json!(
            Self::model(&req).get_by_pk(req.param_i64("id")).await?
        )))
    }

    /// A string-conditioned query, exactly as in the Go original.
    async fn adults(_req: Request) -> Result<Message> {
        let users = Self::model(&_req)
            .query()
            .where_("age >= ?", args![18])
            .order_by_desc("age")
            .list(50)
            .await?;
        Ok(Message::data(json!(users)))
    }

    /// An aggregate projection.
    async fn stats(req: Request) -> Result<Message> {
        #[derive(Debug, serde::Serialize, sea_orm::FromQueryResult)]
        struct Stats {
            count: i64,
            total_balance: Option<i64>,
            average_age: Option<f64>,
        }

        let stats = Self::model(&req)
            .aggregate()
            .select("COUNT(*) AS count, SUM(balance) AS total_balance, AVG(age) AS average_age")
            .row::<Stats>()
            .await?;
        Ok(Message::data(json!(stats)))
    }

    async fn transfer(req: Request) -> Result<Message> {
        #[derive(serde::Deserialize)]
        struct Transfer {
            from: i64,
            to: i64,
            amount: i64,
        }

        let payload: Transfer = req.bind_json()?;
        Self::model(&req)
            .transfer(payload.from, payload.to, payload.amount)
            .await?;
        Ok(Message::with_msg("transferred"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_auto_config()?;
    config.set("web.server.port", json!(19011));

    let mut builder = wf::Builder::new(config);

    // An in-memory database, so the example runs with no setup. Swap in
    // `DbConfig::sqlite("./data.db")` or `DbConfig::mysql(...)` for the real thing.
    let database = connect(&DbConfig::sqlite_memory()).await?;
    builder.model(UserModel::new(database));

    builder.rest(UserController);

    // Seed a couple of rows on the way up.
    builder.routes(|ctx| {
        let model = ctx.try_get::<UserModel>().expect("registered just above");
        ctx.spawn(async move {
            if let Err(error) = model.register("alice@example.com", "alice", 30).await {
                tracing::warn!(%error, "seeding failed");
            }
            if let Err(error) = model.register("bob@example.com", "bob", 17).await {
                tracing::warn!(%error, "seeding failed");
            }
        });
    });

    builder.build().run().await?;
    Ok(())
}

/// A page the example uses when nothing is supplied.
#[allow(dead_code)]
fn default_page() -> Page {
    Page::new(1, 10)
}

/// Kept so the `Response` import stays exercised as the controller grows.
#[allow(dead_code)]
fn no_content() -> Response {
    Response::empty()
}
