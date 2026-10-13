//! The model layer: sea-orm, wired into the container.
//!
//! Queries are sea-orm's own — typed columns, `filter(Column::Age.gte(18))`, `paginate`,
//! `select_only`, `find_with_related`. The framework only adds the connection handle, the
//! table creation, and a place in the container.
//!
//! Runs on an in-memory SQLite database, so it needs no setup:
//!
//! ```text
//! cargo run --example model
//! curl http://localhost:19011/users
//! curl http://localhost:19011/users/adults
//! curl http://localhost:19011/users/stats
//! curl -X POST http://localhost:19011/users/transfer \
//!      -H 'content-type: application/json' -d '{"from":1,"to":2,"amount":5}'
//! ```

use async_trait::async_trait;
use rust_web_frame as wf;
use rust_web_frame::config::load_auto_config;
use rust_web_frame::core::{Context, Rest};
use rust_web_frame::db::{DbConfig, connect};
use rust_web_frame::define_model;
use rust_web_frame::web::{Error, Message, Request, Result, handler};
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::{QueryFilter, QueryOrder, QuerySelect, Set};
use serde_json::json;

// ---------------------------------------------------------------- the entity
//
// The table name lives here, and nowhere else.

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
// `define_model!` writes the handle and the container glue. Everything below it is
// ordinary sea-orm.

define_model!(
    /// Users of the shop.
    UserModel,
    user::Entity
);

impl UserModel {
    /// Business logic sits next to the entity it works on.
    pub async fn register(&self, email: &str, name: &str, age: i32) -> Result<user::Model> {
        Ok(user::ActiveModel {
            email: Set(email.to_owned()),
            name: Set(name.to_owned()),
            age: Set(age),
            balance: Set(0),
            ..Default::default()
        }
        .insert(self.db())
        .await?)
    }

    /// A filtered list, ordered the way sea-orm orders things.
    pub async fn adults(&self) -> Result<Vec<user::Model>> {
        Ok(user::Entity::find()
            .filter(user::Column::Age.gte(18))
            .order_by_desc(user::Column::Age)
            .all(self.db())
            .await?)
    }

    /// Transfers money between two accounts atomically.
    ///
    /// `db()` is the pool outside the closure and the transaction inside it, so the same
    /// methods run either way.
    pub async fn transfer(&self, from: i64, to: i64, amount: i64) -> Result<()> {
        let pool = self.db().clone();

        pool.transaction(|tx| {
            let inside = self.with_db(tx);
            async move {
                let payer = user::Entity::find_by_id(from)
                    .one(inside.db())
                    .await?
                    .ok_or_else(|| {
                        Error::not_found().detail(format!("user {from} does not exist"))
                    })?;

                if payer.balance < amount {
                    return Err(Error::validation_failed().detail("insufficient funds"));
                }

                // `SET balance = balance ± ?` in the database, rather than reading the row
                // back, adding in Rust, and writing what might by then be a stale value.
                for (id, delta) in [(from, -amount), (to, amount)] {
                    user::Entity::update_many()
                        .col_expr(
                            user::Column::Balance,
                            Expr::col(user::Column::Balance).add(delta),
                        )
                        .filter(user::Column::Id.eq(id))
                        .exec(inside.db())
                        .await?;
                }

                Ok(())
            }
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
        ctx.get("/users/adults", handler(Self::adults));
        ctx.get("/users/stats", handler(Self::stats));
        ctx.get("/users/:id", handler(Self::show));
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

    /// Pagination goes through `Model::page`, which feeds the count and the rows to the
    /// framework's response envelope.
    async fn list(req: Request) -> Result<Message> {
        let page = req.page()?;
        let users = Self::model(&req);
        let paged = users
            .page(user::Entity::find().order_by_desc(user::Column::Id), &page)
            .await?;
        Ok(Message::data(json!(paged)))
    }

    async fn adults(req: Request) -> Result<Message> {
        Ok(Message::data(json!(Self::model(&req).adults().await?)))
    }

    async fn show(req: Request) -> Result<Message> {
        let id = req.param_i64("id");
        let users = Self::model(&req);
        let user = user::Entity::find_by_id(id)
            .one(users.db())
            .await?
            .ok_or_else(|| Error::not_found().detail(format!("user {id} does not exist")))?;
        Ok(Message::data(json!(user)))
    }

    /// An aggregate projection, written the way sea-orm writes them.
    async fn stats(req: Request) -> Result<Message> {
        #[derive(Debug, serde::Serialize, sea_orm::FromQueryResult)]
        struct Stats {
            users: i64,
            total_balance: Option<i64>,
            average_age: Option<f64>,
        }

        let users = Self::model(&req);
        let stats = user::Entity::find()
            .select_only()
            .column_as(user::Column::Id.count(), "users")
            .column_as(user::Column::Balance.sum(), "total_balance")
            .column_as(user::Column::Age.avg(), "average_age")
            .into_model::<Stats>()
            .one(users.db())
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
        Ok(Message::with_message("transferred"))
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
        let model = ctx.service::<UserModel>().expect("registered just above");
        ctx.spawn(async move {
            for (email, name, age) in [
                ("alice@example.com", "alice", 30),
                ("bob@example.com", "bob", 17),
            ] {
                if let Err(error) = model.register(email, name, age).await {
                    tracing::warn!(%error, "seeding failed");
                }
            }
        });
    });

    builder.build().run().await?;
    Ok(())
}
