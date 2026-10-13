//! The model layer: sea-orm, wired into the container.
//!
//! Queries are **sea-orm's**, not a dialect of them. `Entity::find()`,
//! `filter(Column::Age.gte(18))`, `insert`, `update_many`, `paginate`,
//! `find_with_related`, `select_only` — all of it works unchanged against the [`Database`]
//! handle this layer hands out, because [`Database`] implements sea-orm's
//! `ConnectionTrait`. Nothing here re-implements a query builder, so the compiler checks
//! column names and types at the call site and the sea-orm documentation applies as
//! written.
//!
//! What the layer adds is the three things sea-orm cannot know about:
//!
//! * **A connection that is either the pool or a transaction.** `Database` is a handle,
//!   so the same `model.db()` works inside and outside
//!   [`Database::transaction`].
//! * **Schema from the entity.** [`Model::create_table`] asks sea-orm's `Schema` to derive
//!   `CREATE TABLE` from the entity, so the table name lives in one place — the
//!   `#[sea_orm(table_name = "…")]` attribute — instead of being passed around.
//! * **A place in the container.** [`TableEntity`] is what
//!   [`Builder::model`](crate::Builder::model) registers, so a handler reaches its model
//!   with [`Context::model`](crate::core::Context::model) instead of rebuilding it.
//!
//! ```
//! use rust_web_frame::define_model;
//! use rust_web_frame::web::Result;
//! use sea_orm::entity::prelude::*;
//! use sea_orm::{ColumnTrait, QueryFilter, QueryOrder, Set};
//!
//! #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
//! #[sea_orm(table_name = "t_user")]
//! pub struct Model {
//!     #[sea_orm(primary_key)]
//!     pub id: i64,
//!     pub email: String,
//!     pub age: i32,
//! }
//!
//! #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
//! pub enum Relation {}
//!
//! impl ActiveModelBehavior for ActiveModel {}
//!
//! define_model!(
//!     /// Users of the shop.
//!     UserModel,
//!     Entity
//! );
//!
//! impl UserModel {
//!     /// Business logic sits next to the entity it works on, written in sea-orm.
//!     pub async fn register(&self, email: &str, age: i32) -> Result<Model> {
//!         Ok(ActiveModel {
//!             email: Set(email.to_owned()),
//!             age: Set(age),
//!             ..Default::default()
//!         }
//!         .insert(self.db())
//!         .await?)
//!     }
//!
//!     pub async fn adults(&self) -> Result<Vec<Model>> {
//!         Ok(Entity::find()
//!             .filter(Column::Age.gte(18))
//!             .order_by_desc(Column::Id)
//!             .all(self.db())
//!             .await?)
//!     }
//! }
//! ```
//!
//! `use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};` is what brings `gte`,
//! `filter` and `order_by_desc` into scope — the same imports the entity would need
//! without this framework in the picture.

use std::marker::PhantomData;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, EntityTrait, PaginatorTrait, Schema, Select};

use crate::db::Database;
pub use crate::util::{Page, Paged};
use crate::web::Result;

/// Builds the bound parameters for raw SQL, where the values are of mixed types and a
/// plain array literal cannot infer one:
///
/// ```
/// # use rust_web_frame::args;
/// use sea_orm::{DbBackend, Statement};
/// let statement = Statement::from_sql_and_values(
///     DbBackend::Sqlite,
///     "SELECT * FROM t_user WHERE age > ? AND city = ?",
///     args![18, "Beijing"],
/// );
/// ```
#[macro_export]
macro_rules! args {
    () => { ::std::vec::Vec::<::sea_orm::Value>::new() };
    ($($value:expr),+ $(,)?) => {
        ::std::vec![$(::sea_orm::Value::from($value)),+]
    };
}

/// An entity plus the connection to run it against.
///
/// `Model<E>` is deliberately thin: it holds the [`Database`] and offers the operations
/// sea-orm's entity API cannot express on its own — deriving the table from the entity,
/// binding the entity to a transaction, and turning a sea-orm page into the framework's
/// response envelope. Everything else is `E::find()`.
pub struct Model<E: EntityTrait> {
    db: Database,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Clone for Model<E> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            _entity: PhantomData,
        }
    }
}

impl<E: EntityTrait> std::fmt::Debug for Model<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("table", &self.table_name())
            .field("inTransaction", &self.db.is_transaction())
            .finish()
    }
}

impl<E: EntityTrait> Model<E> {
    /// Binds entity `E` to a connection.
    ///
    /// The table name is the entity's own, from its `#[sea_orm(table_name = "…")]`
    /// attribute — there is no second place to keep it in step.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            db,
            _entity: PhantomData,
        }
    }

    /// The connection to hand to sea-orm: `Entity::find().all(model.db())`.
    #[must_use]
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The same entity bound to another connection — a transaction, typically.
    ///
    /// ```no_run
    /// # use rust_web_frame::model::Model;
    /// # use rust_web_frame::web::Result;
    /// # mod user {
    /// #     use sea_orm::entity::prelude::*;
    /// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// #     #[sea_orm(table_name = "t_user")]
    /// #     pub struct Model { #[sea_orm(primary_key)] pub id: i64 }
    /// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
    /// #     impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(model: Model<user::Entity>) -> Result<()> {
    /// model
    ///     .db()
    ///     .transaction(|tx| {
    ///         let inside = model.with_db(tx);
    ///         async move {
    ///             let _ = inside.db();
    ///             Ok(())
    ///         }
    ///     })
    ///     .await
    /// # }
    /// ```
    #[must_use]
    pub fn with_db(&self, db: Database) -> Self {
        Self::new(db)
    }

    /// The table this entity maps to.
    #[must_use]
    pub fn table_name(&self) -> &'static str {
        E::default().as_str()
    }

    /// Creates the table when it does not exist, deriving the schema from `E`.
    ///
    /// # Errors
    ///
    /// Fails when the database rejects the statement.
    pub async fn create_table(&self) -> Result<()> {
        let backend = self.db.get_database_backend();
        let statement = Schema::new(backend)
            .create_table_from_entity(E::default())
            .if_not_exists()
            .to_owned();
        self.db.execute_raw(backend.build(&statement)).await?;
        tracing::debug!(table = self.table_name(), "table ready");
        Ok(())
    }

    /// Drops the table and everything in it.
    ///
    /// # Errors
    ///
    /// Fails when the database rejects the statement.
    pub async fn drop_table(&self) -> Result<()> {
        let statement = format!("DROP TABLE IF EXISTS \"{}\"", self.table_name());
        self.db.execute_unprepared(&statement).await?;
        Ok(())
    }

    /// Runs a sea-orm select as one page, in the framework's response envelope.
    ///
    /// The select is sea-orm's, so filters and ordering compose exactly as they do
    /// anywhere else:
    ///
    /// ```no_run
    /// # use rust_web_frame::model::{Model, Page, Paged};
    /// # mod user {
    /// #     use sea_orm::entity::prelude::*;
    /// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// #     #[sea_orm(table_name = "t_user")]
    /// #     pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub age: i32 }
    /// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
    /// #     impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(users: Model<user::Entity>, page: Page)
    /// #     -> Result<(), rust_web_frame::web::Error> {
    /// use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    ///
    /// let rows: Paged<user::Model> = users
    ///     .page(user::Entity::find().filter(user::Column::Age.gte(18)), &page)
    ///     .await?;
    /// # let _ = rows;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when either the count or the page query fails.
    pub async fn page(&self, select: Select<E>, page: &Page) -> Result<Paged<E::Model>>
    where
        E::Model: Send + Sync,
    {
        let page = page.with_defaults();
        let paginator = select.paginate(&self.db, page.page_size());
        let total = paginator.num_items().await?;
        let items = paginator.fetch_page(page.page_index()).await?;
        Ok(Paged::new(total as i64, items))
    }

    /// How many rows a sea-orm select matches.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn count(&self, select: Select<E>) -> Result<i64>
    where
        E::Model: Send + Sync,
    {
        Ok(select.paginate(&self.db, 1).num_items().await? as i64)
    }
}

/// Marks a type as the application's model for an entity, so the framework can create its
/// table at startup and hand it out through the container.
///
/// Implement it by hand, or let [`define_model!`](crate::define_model) write it:
///
/// ```ignore
/// impl TableEntity for UserModel {
///     type Entity = user::Entity;
///     fn model(&self) -> &Model<user::Entity> { &self.inner }
/// }
/// ```
pub trait TableEntity: Send + Sync + 'static {
    /// The sea-orm entity this model wraps.
    type Entity: EntityTrait;

    /// The wrapped handle.
    fn model(&self) -> &Model<Self::Entity>;
}

/// The object-safe view of a model: what the container needs in order to prepare a table.
///
/// You never implement `Table` directly — implementing [`TableEntity`] gets you this for
/// free. The split exists because a `dyn Table` cannot name an unspecified associated
/// type.
#[async_trait]
pub trait Table: Send + Sync + 'static {
    /// The table this model reads and writes.
    fn table_name(&self) -> &'static str;

    /// Creates the table when it does not exist yet.
    async fn create_table(&self) -> Result<()>;

    /// Drops the table and everything in it.
    async fn drop_table(&self) -> Result<()>;
}

#[async_trait]
impl<T: TableEntity> Table for T {
    fn table_name(&self) -> &'static str {
        self.model().table_name()
    }

    async fn create_table(&self) -> Result<()> {
        self.model().create_table().await
    }

    async fn drop_table(&self) -> Result<()> {
        self.model().drop_table().await
    }
}

/// Declares an application model over a sea-orm entity.
///
/// It writes the handle, a [`Deref`](std::ops::Deref) that exposes [`Model`]'s methods,
/// and the [`TableEntity`] impl the container registers. Business logic goes in a separate
/// `impl` block, written with sea-orm's API.
///
/// ```
/// # use rust_web_frame::define_model;
/// # use sea_orm::entity::prelude::*;
/// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// # #[sea_orm(table_name = "t_user")]
/// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub email: String }
/// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
/// # impl ActiveModelBehavior for ActiveModel {}
/// use sea_orm::QueryFilter;
///
/// define_model!(
///     /// Users of the shop.
///     UserModel,
///     Entity
/// );
///
/// impl UserModel {
///     /// Business logic sits next to the entity it works on.
///     pub async fn find_by_email(
///         &self,
///         email: &str,
///     ) -> rust_web_frame::web::Result<Option<Model>> {
///         Ok(Entity::find()
///             .filter(Column::Email.eq(email))
///             .one(self.db())
///             .await?)
///     }
/// }
/// ```
#[macro_export]
macro_rules! define_model {
    ($(#[$attr:meta])* $name:ident, $entity:ty) => {
        $(#[$attr])*
        pub struct $name {
            inner: $crate::model::Model<$entity>,
        }

        impl $name {
            /// Binds this model to `db`.
            pub fn new(db: $crate::db::Database) -> Self {
                Self {
                    inner: $crate::model::Model::new(db),
                }
            }
        }

        impl ::std::ops::Deref for $name {
            type Target = $crate::model::Model<$entity>;

            fn deref(&self) -> &Self::Target {
                &self.inner
            }
        }

        impl $crate::model::TableEntity for $name {
            type Entity = $entity;

            fn model(&self) -> &$crate::model::Model<$entity> {
                &self.inner
            }
        }
    };
}

/// Reports whether a table exists.
///
/// # Errors
///
/// Fails when the database cannot be queried.
pub async fn has_table(db: &Database, table: &str) -> Result<bool> {
    use sea_orm::DbBackend;

    let backend = db.get_database_backend();
    let statement = match backend {
        DbBackend::Sqlite => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            [table.into()],
        ),
        DbBackend::MySql => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?",
            [table.into()],
        ),
        DbBackend::Postgres => sea_orm::Statement::from_sql_and_values(
            backend,
            "SELECT tablename FROM pg_tables \
             WHERE schemaname = current_schema() AND tablename = ?",
            [table.into()],
        ),
        // `DatabaseBackend` is `#[non_exhaustive]`; an unknown driver reports "cannot
        // tell", which makes `create_table` fall through to `CREATE TABLE IF NOT EXISTS`.
        _ => return Ok(false),
    };

    let rows = db.query_all_raw(statement).await?;
    Ok(!rows.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::entity::prelude::*;
    use sea_orm::{QueryFilter, QueryOrder, QuerySelect, Set};

    mod cake {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "t_cake")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i64,
            pub name: String,
            pub price: i64,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    define_model!(CakeModel, cake::Entity);

    async fn setup() -> CakeModel {
        let db = crate::db::connect_memory().await.unwrap();
        let cakes = CakeModel::new(db);
        cakes.create_table().await.unwrap();
        cakes
    }

    async fn seed(cakes: &CakeModel, name: &str, price: i64) -> cake::Model {
        cake::ActiveModel {
            name: Set(name.to_owned()),
            price: Set(price),
            ..Default::default()
        }
        .insert(cakes.db())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_table_name_comes_from_the_entity() {
        let cakes = setup().await;
        assert_eq!(cakes.table_name(), "t_cake");
        assert_eq!(CakeModel::new(cakes.db().clone()).table_name(), "t_cake");
    }

    #[tokio::test]
    async fn create_table_is_idempotent() {
        let cakes = setup().await;
        assert!(has_table(cakes.db(), "t_cake").await.unwrap());
        cakes.create_table().await.unwrap();
    }

    #[tokio::test]
    async fn sea_orm_queries_run_against_the_handle() {
        let cakes = setup().await;
        seed(&cakes, "cheesecake", 12).await;
        seed(&cakes, "victoria", 5).await;

        let cheap = cake::Entity::find()
            .filter(cake::Column::Price.lt(10))
            .order_by_desc(cake::Column::Price)
            .all(cakes.db())
            .await
            .unwrap();
        assert_eq!(cheap.len(), 1);
        assert_eq!(cheap[0].name, "victoria");

        let one = cake::Entity::find_by_id(1i64)
            .one(cakes.db())
            .await
            .unwrap();
        assert_eq!(one.unwrap().name, "cheesecake");
    }

    #[tokio::test]
    async fn a_transaction_handle_carries_the_same_queries() {
        let cakes = setup().await;
        let db = cakes.db().clone();

        db.transaction(|tx| {
            let inside = cakes.with_db(tx);
            async move {
                cake::ActiveModel {
                    name: Set("committed".to_owned()),
                    price: Set(1),
                    ..Default::default()
                }
                .insert(inside.db())
                .await?;
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(cake::Entity::find().count(&db).await.unwrap(), 1);

        let outcome: Result<()> = db
            .transaction(|tx| {
                let inside = cakes.with_db(tx);
                async move {
                    cake::ActiveModel {
                        name: Set("rolled back".to_owned()),
                        price: Set(2),
                        ..Default::default()
                    }
                    .insert(inside.db())
                    .await?;
                    Err(crate::web::Error::internal().detail("boom"))
                }
            })
            .await;
        assert!(outcome.is_err());
        assert_eq!(cake::Entity::find().count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_page_reports_the_total_and_the_rows() {
        let cakes = setup().await;
        for i in 0..25 {
            seed(&cakes, &format!("cake-{i}"), i).await;
        }

        let ordered = || cake::Entity::find().order_by_desc(cake::Column::Id);

        let first = cakes.page(ordered(), &Page::new(1, 10)).await.unwrap();
        assert_eq!(first.total, 25);
        assert_eq!(first.items.len(), 10);
        assert_eq!(first.items[0].name, "cake-24");

        let last = cakes.page(ordered(), &Page::new(3, 10)).await.unwrap();
        assert_eq!(last.items.len(), 5);

        // The filter is sea-orm's, so the total counts only what matched.
        let filtered = cakes
            .page(
                cake::Entity::find().filter(cake::Column::Price.gte(20)),
                &Page::new(1, 10),
            )
            .await
            .unwrap();
        assert_eq!(filtered.total, 5);
    }

    #[tokio::test]
    async fn counting_a_select_is_sea_orms() {
        let cakes = setup().await;
        for price in [1, 5, 9] {
            seed(&cakes, "cake", price).await;
        }
        let all = cakes.count(cake::Entity::find()).await.unwrap();
        let dear = cakes
            .count(cake::Entity::find().filter(cake::Column::Price.gte(5)))
            .await
            .unwrap();
        assert_eq!((all, dear), (3, 2));
    }

    #[tokio::test]
    async fn aggregates_are_sea_orms() {
        let cakes = setup().await;
        for price in [1, 2, 3] {
            seed(&cakes, "cake", price).await;
        }

        let sum = cake::Entity::find()
            .select_only()
            .column_as(cake::Column::Price.sum(), "total")
            .into_tuple::<i64>()
            .one(cakes.db())
            .await
            .unwrap();
        assert_eq!(sum, Some(6));

        #[derive(Debug, sea_orm::FromQueryResult)]
        struct Summary {
            count: i64,
            total: Option<i64>,
        }

        let summary = cake::Entity::find()
            .select_only()
            .column_as(cake::Column::Id.count(), "count")
            .column_as(cake::Column::Price.sum(), "total")
            .into_model::<Summary>()
            .one(cakes.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((summary.count, summary.total), (3, Some(6)));
    }

    #[tokio::test]
    async fn dropping_the_table_removes_it() {
        let cakes = setup().await;
        seed(&cakes, "a", 1).await;
        cakes.drop_table().await.unwrap();
        assert!(!has_table(cakes.db(), "t_cake").await.unwrap());
    }

    #[tokio::test]
    async fn the_object_safe_table_view_forwards_to_the_model() {
        let cakes = setup().await;
        let as_table: &dyn Table = &cakes;
        assert_eq!(as_table.table_name(), "t_cake");
        as_table.create_table().await.unwrap();
        as_table.drop_table().await.unwrap();
    }
}
