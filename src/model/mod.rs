//! The generic model layer — CRUD without boilerplate.
//!
//! One sea-orm entity gives you a typed model:
//!
//! ```no_run
//! # mod entity {
//! # use sea_orm::entity::prelude::*;
//! #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
//! #[sea_orm(table_name = "t_user")]
//! pub struct Model {
//!     #[sea_orm(primary_key)]
//!     pub id: i64,
//!     pub name: String,
//!     pub age: i32,
//! }
//! #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
//! pub enum Relation {}
//! impl ActiveModelBehavior for ActiveModel {}
//! # }
//! # use rust_web_frame::model::{EntryModel, Page};
//! # use rust_web_frame::web::Result;
//! async fn demo(db: rust_web_frame::db::Database) -> Result<()> {
//!     let users = EntryModel::<entity::Entity>::new(db, "t_user");
//!     users.create_table().await?;
//!
//!     let alice = users.find_by_pk(1).await?;                       // Option<entity::Model>
//!     let all = users.find_all().await?;                            // Vec<entity::Model>
//!     let adults = users
//!         .query()
//!         .where_("age > ?", rust_web_frame::args![18])
//!         .all()
//!         .await?;
//!     let (page, total) = users.page(&Page::new(1, 10)).await?;
//!     users.update().where_pk(1).set("name", "bob").exec().await?;
//!     users.delete_by_pk(1).await?;
//!     Ok(())
//! }
//! ```

mod query;

use std::marker::PhantomData;

use sea_orm::{ConnectionTrait, EntityTrait, IntoActiveModel, PrimaryKeyTrait, Select};

use crate::db::Database;
pub use crate::util::{Page, Paged};
use crate::web::{Error, Result};

/// Re-exported so a model file only needs one import for its bind parameters.
pub use crate::args;
pub use query::{Aggregate, Delete, Query, Update, primary_key_name};

/// A typed model bound to a table.
///
/// `Model<E>` is the plain CRUD surface; [`EntryModel<E>`] adds primary-key shortcuts.
/// Both are cheap handles over a connection pool — clone or renew them freely.
pub struct Model<E: EntityTrait> {
    db: Database,
    table: String,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Clone for Model<E> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            table: self.table.clone(),
            _entity: PhantomData,
        }
    }
}

impl<E: EntityTrait> std::fmt::Debug for Model<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("table", &self.table)
            .finish_non_exhaustive()
    }
}

impl<E: EntityTrait> Model<E> {
    /// Binds entity `E` to `table`.
    pub fn new(db: Database, table: impl Into<String>) -> Self {
        Self {
            db,
            table: table.into(),
            _entity: PhantomData,
        }
    }

    /// The connection pool.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// The table name.
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// Binds the same table to a different connection — a transaction, typically.
    pub fn with_db(&self, db: Database) -> Self {
        Self {
            db,
            table: self.table.clone(),
            _entity: PhantomData,
        }
    }

    /// Alias of [`with_db`](Self::with_db), mirroring the Go original's `ReNew`.
    pub fn renew(&self, db: Database) -> Self {
        self.with_db(db)
    }

    /// Reports whether the table exists.
    pub async fn is_exist(&self) -> Result<bool> {
        crate::db::has_table(&self.db, &self.table).await
    }

    /// Creates the table when it does not exist yet, deriving the schema from `E`.
    ///
    /// ```no_run
    /// # mod entity {
    /// #     use sea_orm::entity::prelude::*;
    /// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// #     #[sea_orm(table_name = "t_user")]
    /// #     pub struct Model { #[sea_orm(primary_key)] pub id: i64 }
    /// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    /// #     pub enum Relation {}
    /// #     impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(m: rust_web_frame::model::Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
    /// m.create_table().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_table(&self) -> Result<()> {
        if self.is_exist().await? {
            return Ok(());
        }
        use sea_orm::Schema;
        let backend = self.db.get_database_backend();
        let schema = Schema::new(backend);
        let statement = schema
            .create_table_from_entity(E::default())
            .if_not_exists()
            .to_owned();
        self.db
            .execute_raw(backend.build(&statement))
            .await
            .map_err(Error::from)?;
        tracing::debug!(table = %self.table, "table created");
        Ok(())
    }

    /// Drops the table and everything in it.
    pub async fn drop_table(&self) -> Result<()> {
        crate::db::drop_table_if_exists(&self.db, &self.table).await
    }

    /// Deletes every row, keeping the table itself.
    pub async fn delete_all(&self) -> Result<u64> {
        self.delete().exec().await
    }

    /// Inserts one row and returns it, primary key included.
    ///
    /// ```no_run
    /// # mod entity {
    /// # use sea_orm::entity::prelude::*;
    /// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// # #[sea_orm(table_name = "t_user")]
    /// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
    /// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
    /// # impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(m: rust_web_frame::model::Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
    /// let saved = m.insert(entity::ActiveModel {
    ///     name: sea_orm::Set("alice".into()),
    ///     ..Default::default()
    /// }).await?;
    /// assert!(saved.id > 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn insert(&self, model: E::ActiveModel) -> Result<E::Model>
    where
        E::Model: IntoActiveModel<E::ActiveModel>,
    {
        self.ensure_known_table()?;
        E::insert(model)
            .exec_with_returning(&self.db)
            .await
            .map_err(|err| match err {
                sea_orm::DbErr::RecordNotInserted => {
                    Error::duplicate_entry().detail(format!("{} already has that row", self.table))
                }
                other => Error::from(other),
            })
    }

    /// Inserts many rows in one statement, returning how many were written.
    pub async fn insert_many(&self, models: Vec<E::ActiveModel>) -> Result<u64> {
        self.ensure_known_table()?;
        E::insert_many(models)
            .exec_without_returning(&self.db)
            .await
            .map_err(|err| match err {
                sea_orm::DbErr::RecordNotInserted => {
                    Error::duplicate_entry().detail(format!("{} already has that row", self.table))
                }
                other => Error::from(other),
            })
    }

    /// Inserts a row built from a map of column names to values.
    pub async fn save_map(&self, values: Vec<(&str, sea_orm::Value)>) -> Result<u64> {
        use sea_orm::sea_query::{Alias, Expr as SeaExpr, Query as SeaQuery};

        let mut query = SeaQuery::insert();
        query
            .into_table(Alias::new(self.table.as_str()))
            .columns(values.iter().map(|(column, _)| Alias::new(*column)));
        query
            .values(values.into_iter().map(|(_, value)| SeaExpr::value(value)))
            .map_err(|err| Error::new(500, err.to_string()))?;

        let backend = self.db.get_database_backend();
        let result = self
            .db
            .execute_raw(backend.build(&query))
            .await
            .map_err(Error::from)?;
        Ok(result.rows_affected())
    }

    /// Starts a read query.
    pub fn query(&self) -> Query<E> {
        Query::new(self.db.clone())
    }

    /// Starts an update.
    pub fn update(&self) -> Update<E> {
        Update::new(self.db.clone(), self.table.clone())
    }

    /// Starts a delete.
    pub fn delete(&self) -> Delete<E> {
        Delete::new(self.db.clone(), self.table.clone())
    }

    /// Starts an aggregate query.
    pub fn aggregate(&self) -> Aggregate<E> {
        Aggregate::new(self.db.clone())
    }

    /// The `SELECT` statement for this entity, for the cases the fluent builder does not
    /// cover.
    pub fn select(&self) -> Select<E> {
        E::find()
    }

    fn ensure_known_table(&self) -> Result<()> {
        if self.table.is_empty() {
            return Err(Error::internal().detail("model has no table name"));
        }
        Ok(())
    }
}

/// A [`Model`] that knows its primary key, and therefore offers the lookups an
/// application writes most often.
///
/// ```no_run
/// # mod entity {
/// # use sea_orm::entity::prelude::*;
/// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// # #[sea_orm(table_name = "t_user")]
/// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
/// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
/// # impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # async fn demo(db: rust_web_frame::db::Database) -> Result<(), rust_web_frame::web::Error> {
/// let users = rust_web_frame::model::EntryModel::<entity::Entity>::new(db, "t_user");
/// let user = users.find_by_pk(7).await?;
/// let all = users.find_all().await?;
/// users.delete_by_pk(7).await?;
/// # Ok(())
/// # }
/// ```
pub struct EntryModel<E: EntityTrait> {
    inner: Model<E>,
}

impl<E: EntityTrait> Clone for EntryModel<E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<E: EntityTrait> std::fmt::Debug for EntryModel<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryModel")
            .field("table", &self.inner.table)
            .finish_non_exhaustive()
    }
}

impl<E: EntityTrait> EntryModel<E> {
    /// Binds entity `E` to `table`.
    pub fn new(db: Database, table: impl Into<String>) -> Self {
        Self {
            inner: Model::new(db, table),
        }
    }

    /// The underlying [`Model`], for the operations that are not primary-key specific.
    pub fn model(&self) -> &Model<E> {
        &self.inner
    }

    /// The connection pool.
    pub fn db(&self) -> &Database {
        self.inner.db()
    }

    /// The table name.
    pub fn table_name(&self) -> &str {
        self.inner.table_name()
    }

    /// Binds the same table to a different connection — a transaction, typically.
    pub fn with_db(&self, db: Database) -> Self {
        Self {
            inner: self.inner.with_db(db),
        }
    }

    /// Alias of [`with_db`](Self::with_db).
    pub fn renew(&self, db: Database) -> Self {
        self.with_db(db)
    }

    /// Reports whether the table exists.
    pub async fn is_exist(&self) -> Result<bool> {
        self.inner.is_exist().await
    }

    /// Creates the table when it does not exist yet.
    pub async fn create_table(&self) -> Result<()> {
        self.inner.create_table().await
    }

    /// Drops the table and everything in it.
    pub async fn drop_table(&self) -> Result<()> {
        self.inner.drop_table().await
    }

    /// Deletes every row, keeping the table itself.
    pub async fn delete_all(&self) -> Result<u64> {
        self.inner.delete_all().await
    }

    /// Inserts one row and returns it, primary key included.
    pub async fn insert(&self, model: E::ActiveModel) -> Result<E::Model>
    where
        E::Model: IntoActiveModel<E::ActiveModel>,
    {
        self.inner.insert(model).await
    }

    /// Inserts many rows in one statement, returning how many were written.
    pub async fn insert_many(&self, models: Vec<E::ActiveModel>) -> Result<u64> {
        self.inner.insert_many(models).await
    }

    /// Inserts a row built from a map of column names to values.
    pub async fn save_map(&self, values: Vec<(&str, sea_orm::Value)>) -> Result<u64> {
        self.inner.save_map(values).await
    }

    /// Starts a read query.
    pub fn query(&self) -> Query<E> {
        self.inner.query()
    }

    /// Starts an update.
    pub fn update(&self) -> Update<E> {
        self.inner.update()
    }

    /// Starts a delete.
    pub fn delete(&self) -> Delete<E> {
        self.inner.delete()
    }

    /// Starts an aggregate query.
    pub fn aggregate(&self) -> Aggregate<E> {
        self.inner.aggregate()
    }

    /// The `SELECT` statement for this entity.
    pub fn select(&self) -> Select<E> {
        self.inner.select()
    }

    /// Finds a row by primary key.
    pub async fn find_by_pk<V>(&self, id: V) -> Result<Option<E::Model>>
    where
        V: Into<<E::PrimaryKey as PrimaryKeyTrait>::ValueType>,
    {
        E::find_by_id(id).one(self.db()).await.map_err(Error::from)
    }

    /// Finds a row by primary key, failing with `404` when there is none.
    pub async fn get_by_pk<V>(&self, id: V) -> Result<E::Model>
    where
        V: Into<<E::PrimaryKey as PrimaryKeyTrait>::ValueType>,
    {
        self.find_by_pk(id)
            .await?
            .ok_or_else(|| Error::not_found().detail(format!("{} not found", self.table_name())))
    }

    /// Finds rows by a list of primary keys.
    pub async fn find_all_by_pk<V>(&self, ids: impl IntoIterator<Item = V>) -> Result<Vec<E::Model>>
    where
        V: Into<sea_orm::Value>,
    {
        let ids: Vec<sea_orm::Value> = ids.into_iter().map(Into::into).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.query()
            .where_in(&primary_key_name::<E>(), ids)
            .all()
            .await
    }

    /// Returns every row, ordered by primary key.
    pub async fn find_all(&self) -> Result<Vec<E::Model>> {
        self.query()
            .order_by_asc(&primary_key_name::<E>())
            .all()
            .await
    }

    /// Finds the first row matching a condition.
    pub async fn find_one(
        &self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = sea_orm::Value>,
    ) -> Result<Option<E::Model>> {
        self.query().where_(condition, values).one().await
    }

    /// Deletes a row by primary key, returning how many rows were removed.
    pub async fn delete_by_pk<V>(&self, id: V) -> Result<u64>
    where
        V: Into<sea_orm::Value>,
    {
        self.delete().where_pk(id).exec().await
    }

    /// Updates a row, using the primary key carried by `model`.
    ///
    /// ```no_run
    /// # mod entity {
    /// # use sea_orm::entity::prelude::*;
    /// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// # #[sea_orm(table_name = "t_user")]
    /// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
    /// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
    /// # impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(m: rust_web_frame::model::EntryModel<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
    /// m.update_by_pk(entity::ActiveModel {
    ///     id: sea_orm::Set(7),
    ///     name: sea_orm::Set("bob".into()),
    ///     ..Default::default()
    /// }).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn update_by_pk(&self, model: E::ActiveModel) -> Result<E::Model>
    where
        E::Model: IntoActiveModel<E::ActiveModel>,
    {
        E::update(model).exec(self.db()).await.map_err(Error::from)
    }

    /// Updates one column of the row with the given primary key.
    pub async fn update_column<V>(
        &self,
        id: V,
        column: &str,
        value: impl Into<sea_orm::Value>,
    ) -> Result<u64>
    where
        V: Into<sea_orm::Value>,
    {
        self.update().where_pk(id).set(column, value).exec().await
    }

    /// Updates several columns of the row with the given primary key.
    pub async fn update_map<V>(
        &self,
        id: V,
        values: impl IntoIterator<Item = (&'static str, sea_orm::Value)>,
    ) -> Result<u64>
    where
        V: Into<sea_orm::Value>,
    {
        self.update().where_pk(id).set_map(values).exec().await
    }

    /// Returns one page of rows, newest primary key first.
    pub async fn page(&self, page: &Page) -> Result<(Vec<E::Model>, i64)> {
        self.query()
            .order_by_desc(&primary_key_name::<E>())
            .page(page)
            .await
    }

    /// Returns one page of rows wrapped in a [`Paged`].
    pub async fn page_for_web(&self, page: &Page) -> Result<Paged<E::Model>> {
        self.query()
            .order_by_desc(&primary_key_name::<E>())
            .page_for_web(page)
            .await
    }

    /// Returns one page of rows matching a condition.
    pub async fn query_page(
        &self,
        page: &Page,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = sea_orm::Value>,
    ) -> Result<(Vec<E::Model>, i64)> {
        self.query()
            .where_(condition, values)
            .order_by_desc(&primary_key_name::<E>())
            .page(page)
            .await
    }

    /// Counts every row in the table.
    pub async fn count(&self) -> Result<i64> {
        self.query().count().await
    }
}

/// Marks a type as the application-facing model for an entity, so the framework can
/// create its table at startup and hand it out through the dependency-injection context.
///
/// Implement it by hand, or let [`define_model!`](crate::define_model) write the
/// boilerplate:
///
/// ```no_run
/// use rust_web_frame::define_model;
/// use sea_orm::entity::prelude::*;
///
/// #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// #[sea_orm(table_name = "t_user")]
/// pub struct Model {
///     #[sea_orm(primary_key)]
///     pub id: i64,
///     pub name: String,
/// }
/// #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// pub enum Relation {}
/// impl ActiveModelBehavior for ActiveModel {}
///
/// define_model!(UserModel, Entity, "t_user");
///
/// // `UserModel` derefs to `EntryModel<Entity>`, so every query method is available:
/// //   users.find_by_pk(1).await?
/// //   users.query().where_("age > ?", args![18]).all().await?
/// ```
pub trait TableEntity: Send + Sync + 'static {
    /// The sea-orm entity this model wraps.
    type Entity: EntityTrait;

    /// The wrapped entry model.
    fn entry_model(&self) -> &EntryModel<Self::Entity>;
}

/// The object-safe view of a model: what the framework itself needs in order to prepare
/// and manage tables.
///
/// You never implement `Table` directly — implementing [`TableEntity`] gets you this for
/// free.
#[async_trait::async_trait]
pub trait Table: Send + Sync + 'static {
    /// The table this model reads and writes.
    fn table_name(&self) -> &str;

    /// Reports whether the table exists.
    async fn is_exist(&self) -> Result<bool>;

    /// Creates the table when it does not exist yet.
    async fn create_table(&self) -> Result<()>;

    /// Drops the table and everything in it.
    async fn drop_table(&self) -> Result<()>;

    /// Deletes every row, keeping the table itself.
    async fn delete_all(&self) -> Result<u64>;
}

#[async_trait::async_trait]
impl<T: TableEntity> Table for T {
    fn table_name(&self) -> &str {
        self.entry_model().table_name()
    }

    async fn is_exist(&self) -> Result<bool> {
        self.entry_model().is_exist().await
    }

    async fn create_table(&self) -> Result<()> {
        self.entry_model().create_table().await
    }

    async fn drop_table(&self) -> Result<()> {
        self.entry_model().drop_table().await
    }

    async fn delete_all(&self) -> Result<u64> {
        self.entry_model().delete_all().await
    }
}

/// Declares an application model over a sea-orm entity.
///
/// Expands to a newtype wrapping [`EntryModel`], a [`Deref`](std::ops::Deref) impl that
/// exposes every query method, and a [`TableEntity`] impl the framework can register.
/// Add your own methods in a separate `impl` block.
///
/// ```no_run
/// # use rust_web_frame::define_model;
/// # use sea_orm::entity::prelude::*;
/// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// # #[sea_orm(table_name = "t_user")]
/// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
/// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
/// # impl ActiveModelBehavior for ActiveModel {}
/// define_model!(
///     /// Users of the shop.
///     UserModel,
///     Entity,
///     "t_user"
/// );
///
/// impl UserModel {
///     /// Business logic lives next to the queries it uses.
///     pub async fn find_by_name(&self, name: &str) -> rust_web_frame::web::Result<Option<Model>> {
///         self.query().where_eq("name", name).one().await
///     }
/// }
/// ```
#[macro_export]
macro_rules! define_model {
    ($(#[$attr:meta])* $name:ident, $entity:ty, $table:expr) => {
        $(#[$attr])*
        pub struct $name {
            inner: $crate::model::EntryModel<$entity>,
        }

        impl $name {
            /// Binds this model to `db` and to its table.
            pub fn new(db: $crate::db::Database) -> Self {
                Self {
                    inner: $crate::model::EntryModel::new(db, $table),
                }
            }

        }

        impl ::std::ops::Deref for $name {
            type Target = $crate::model::EntryModel<$entity>;

            fn deref(&self) -> &Self::Target {
                &self.inner
            }
        }

        impl $crate::model::TableEntity for $name {
            type Entity = $entity;

            fn entry_model(&self) -> &$crate::model::EntryModel<$entity> {
                &self.inner
            }
        }
    };
}

/// Runs a closure inside a database transaction.
///
/// ```no_run
/// # use rust_web_frame::model::{EntryModel, Transaction};
/// # use rust_web_frame::web::{Error, Result};
/// # mod entity {
/// # use sea_orm::entity::prelude::*;
/// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// # #[sea_orm(table_name = "t_account")]
/// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub balance: i64 }
/// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
/// # impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # async fn transfer(tx: &Transaction, from: i64, to: i64, amount: i64) -> Result<()> {
/// tx.exec(|db| async move {
///     let accounts = EntryModel::<entity::Entity>::new(db, "t_account");
///     accounts.update().where_pk(from).set("balance", -amount).exec().await?;
///     accounts.update().where_pk(to).set("balance", amount).exec().await?;
///     Ok(())
/// })
/// .await
/// # }
/// ```
pub struct Transaction {
    db: Database,
}

impl Transaction {
    /// Wraps a pool.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Runs `body` in a transaction: committed when it returns `Ok`, rolled back
    /// otherwise.
    ///
    /// The handle the closure receives is a [`Database`], so models
    /// built from it participate in the transaction.
    pub async fn exec<F, Fut, T>(&self, body: F) -> Result<T>
    where
        F: FnOnce(Database) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        crate::db::transaction(&self.db, body).await
    }

    /// The pool this transaction manager runs against.
    pub fn db(&self) -> &Database {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Set;

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

    async fn setup() -> EntryModel<cake::Entity> {
        let db = crate::db::connect_memory().await.unwrap();
        let cakes = EntryModel::<cake::Entity>::new(db, "t_cake");
        cakes.create_table().await.unwrap();
        cakes
    }

    #[tokio::test]
    async fn tables_are_created_from_the_entity() {
        let cakes = setup().await;
        assert!(cakes.is_exist().await.unwrap());
        // Creating twice is a no-op, not an error.
        cakes.create_table().await.unwrap();
    }

    #[tokio::test]
    async fn save_find_and_delete_round_trip() {
        let cakes = setup().await;

        let saved = cakes
            .insert(cake::ActiveModel {
                name: Set("cheesecake".into()),
                price: Set(12),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(saved.id > 0);

        let found = cakes.find_by_pk(saved.id).await.unwrap().unwrap();
        assert_eq!(found.name, "cheesecake");
        assert_eq!(cakes.count().await.unwrap(), 1);

        assert_eq!(cakes.delete_by_pk(saved.id).await.unwrap(), 1);
        assert!(cakes.find_by_pk(saved.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn saves_batch_inserts() {
        let cakes = setup().await;
        cakes
            .insert_many(vec![
                cake::ActiveModel {
                    name: Set("a".into()),
                    price: Set(1),
                    ..Default::default()
                },
                cake::ActiveModel {
                    name: Set("b".into()),
                    price: Set(2),
                    ..Default::default()
                },
            ])
            .await
            .unwrap();
        assert_eq!(cakes.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn conditions_orders_and_limits_compose() {
        let cakes = setup().await;
        for (name, price) in [("a", 1), ("b", 5), ("c", 9)] {
            cakes
                .insert(cake::ActiveModel {
                    name: Set(name.into()),
                    price: Set(price),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let expensive = cakes
            .query()
            .where_("price > ?", crate::args![4])
            .order_by_desc("price")
            .all()
            .await
            .unwrap();
        assert_eq!(
            expensive
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "b"]
        );

        assert_eq!(
            cakes
                .query()
                .where_("price > ?", crate::args![4])
                .count()
                .await
                .unwrap(),
            2
        );
        assert_eq!(cakes.query().list(1).await.unwrap().len(), 1);
        assert!(cakes.query().where_eq("name", "a").exists().await.unwrap());
    }

    #[tokio::test]
    async fn where_in_handles_the_empty_case() {
        let cakes = setup().await;
        cakes
            .insert(cake::ActiveModel {
                name: Set("a".into()),
                price: Set(1),
                ..Default::default()
            })
            .await
            .unwrap();

        let none: Vec<i64> = Vec::new();
        assert!(
            cakes
                .query()
                .where_in("id", none)
                .all()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cakes
                .query()
                .where_in("id", [1i64])
                .all()
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn updates_and_deletes_are_conditioned() {
        let cakes = setup().await;
        let saved = cakes
            .insert(cake::ActiveModel {
                name: Set("a".into()),
                price: Set(1),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(cakes.update_column(saved.id, "price", 42).await.unwrap(), 1);
        assert_eq!(cakes.find_by_pk(saved.id).await.unwrap().unwrap().price, 42);

        assert_eq!(
            cakes
                .update()
                .where_("price > ?", crate::args![100])
                .set("name", "x")
                .exec()
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            cakes
                .delete()
                .where_("price > ?", crate::args![100])
                .exec()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn paging_reports_the_total() {
        let cakes = setup().await;
        for i in 0..25 {
            cakes
                .insert(cake::ActiveModel {
                    name: Set(format!("cake-{i}")),
                    price: Set(i),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let (first, total) = cakes.page(&Page::new(1, 10)).await.unwrap();
        assert_eq!(total, 25);
        assert_eq!(first.len(), 10);
        // Newest primary key first.
        assert_eq!(first[0].name, "cake-24");

        let (third, _) = cakes.page(&Page::new(3, 10)).await.unwrap();
        assert_eq!(third.len(), 5);

        let wrapped = cakes.page_for_web(&Page::new(1, 5)).await.unwrap();
        assert_eq!(wrapped.total, 25);
        assert_eq!(wrapped.list.len(), 5);
    }

    #[tokio::test]
    async fn aggregates_project_into_query_results() {
        let cakes = setup().await;
        for price in [1, 2, 3] {
            cakes
                .insert(cake::ActiveModel {
                    name: Set(format!("cake-{price}")),
                    price: Set(price),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let total: Option<i64> = cakes
            .aggregate()
            .select("SUM(price)")
            .value()
            .await
            .unwrap();
        assert_eq!(total, Some(6));

        let count: Option<i64> = cakes.aggregate().select("COUNT(*)").value().await.unwrap();
        assert_eq!(count, Some(3));

        let max: Option<i64> = cakes
            .aggregate()
            .select("MAX(price)")
            .where_("price > ?", crate::args![1])
            .value()
            .await
            .unwrap();
        assert_eq!(max, Some(3));
    }

    #[tokio::test]
    async fn transactions_commit_and_roll_back() {
        let cakes = setup().await;
        let db = cakes.db().clone();
        let tx = Transaction::new(db.clone());

        tx.exec(|conn| async move {
            let inner = EntryModel::<cake::Entity>::new(conn.clone(), "t_cake");
            inner
                .insert(cake::ActiveModel {
                    name: Set("committed".into()),
                    price: Set(1),
                    ..Default::default()
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(cakes.count().await.unwrap(), 1);

        let result: Result<()> = tx
            .exec(|conn| async move {
                let inner = EntryModel::<cake::Entity>::new(conn.clone(), "t_cake");
                inner
                    .insert(cake::ActiveModel {
                        name: Set("rolled back".into()),
                        price: Set(2),
                        ..Default::default()
                    })
                    .await?;
                Err(Error::internal().detail("boom"))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(cakes.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn missing_rows_are_none_not_an_error() {
        let cakes = setup().await;
        assert!(cakes.find_by_pk(999).await.unwrap().is_none());
        assert_eq!(cakes.get_by_pk(999).await.unwrap_err().code, 404);
    }

    #[tokio::test]
    async fn renew_rebinds_to_another_connection() {
        let cakes = setup().await;
        let other = crate::db::connect_memory().await.unwrap();
        let rebound = cakes.renew(other);
        assert_eq!(rebound.table_name(), "t_cake");
        // The new connection has no such table.
        assert!(rebound.count().await.is_err());
    }
}
