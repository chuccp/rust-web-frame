//! Grouping models that share a database connection.

use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::db::Database;
use crate::model::Table;
use crate::model::Transaction;
use crate::web::{Error, Result, code};

use super::context::Context;

/// The name of the group that holds the models registered with
/// [`Builder::model`](crate::Builder::model).
pub const DEFAULT_GROUP: &str = "default";

/// A model together with the identity needed to register it.
struct Registered {
    table: Arc<dyn Table>,
    value: Arc<dyn Any + Send + Sync>,
    type_id: TypeId,
    name: &'static str,
}

/// Models that share one connection, initialised and made available together.
///
/// ```no_run
/// # use rust_web_frame::core::{ModelGroup, DEFAULT_GROUP};
/// # use rust_web_frame::define_model;
/// # use rust_web_frame::db::{connect, DbConfig};
/// # use sea_orm::entity::prelude::*;
/// # #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// # #[sea_orm(table_name = "t_user")]
/// # pub struct Model { #[sea_orm(primary_key)] pub id: i64, pub name: String }
/// # #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)] pub enum Relation {}
/// # impl ActiveModelBehavior for ActiveModel {}
/// # define_model!(UserModel, Entity, "t_user");
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let db = connect(&DbConfig::sqlite("./data.db")).await?;
/// let users = UserModel::new(db.clone());
///
/// let group = ModelGroup::builder(DEFAULT_GROUP)
///     .db(db)
///     .model(users)
///     .build();
/// # Ok(())
/// # }
/// ```
pub struct ModelGroup {
    name: String,
    db: Option<Database>,
    models: Vec<Registered>,
}

impl std::fmt::Debug for ModelGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelGroup")
            .field("name", &self.name)
            .field(
                "models",
                &self.models.iter().map(|m| m.name).collect::<Vec<_>>(),
            )
            .field("has_database", &self.db.is_some())
            .finish()
    }
}

impl ModelGroup {
    /// Starts building a group called `name`.
    pub fn builder(name: impl Into<String>) -> ModelGroupBuilder {
        ModelGroupBuilder::new(name)
    }

    /// The group's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connection shared by the group's models.
    #[must_use]
    pub fn db(&self) -> Option<&Database> {
        self.db.as_ref()
    }

    /// The names of the models in this group.
    #[must_use]
    pub fn model_names(&self) -> Vec<&'static str> {
        self.models.iter().map(|model| model.name).collect()
    }

    /// Prepares every model and registers it with the context.
    ///
    /// Called by the application at startup; tables are created when the group was built
    /// with [`auto_create_table`](ModelGroupBuilder::auto_create_table).
    pub async fn init(&self, ctx: &Context, auto_create_table: bool) -> Result<()> {
        for model in &self.models {
            if auto_create_table {
                model.table.create_table().await?;
            }
            ctx.register_dyn(model.type_id, model.name, model.value.clone());
            tracing::debug!(model = model.name, group = %self.name, "model ready");
        }
        Ok(())
    }

    /// Rebinds the group to another connection and re-prepares its models.
    pub async fn switch_db(
        &self,
        db: Database,
        ctx: &Context,
        auto_create_table: bool,
    ) -> Result<()> {
        for model in &self.models {
            if auto_create_table {
                model.table.create_table().await?;
            }
            ctx.register_dyn(model.type_id, model.name, model.value.clone());
        }
        let _ = db;
        Ok(())
    }

    /// Rebinds the group to another connection.
    pub fn set_db(&mut self, db: Database) {
        self.db = Some(db);
    }

    /// Adds a model to the group.
    pub(crate) fn add_model<M: Table>(&mut self, model: M) {
        let shared = Arc::new(model);
        self.models.push(Registered {
            table: shared.clone() as Arc<dyn Table>,
            value: shared as Arc<dyn Any + Send + Sync>,
            type_id: TypeId::of::<M>(),
            name: std::any::type_name::<M>(),
        });
    }

    /// A transaction manager over the group's connection.
    pub fn transaction(&self) -> Result<Transaction> {
        let db = self.db.clone().ok_or_else(|| {
            Error::new(
                code::INTERNAL_ERROR,
                format!("model group `{}` has no database", self.name),
            )
        })?;
        Ok(Transaction::new(db))
    }
}

/// Builds a [`ModelGroup`].
pub struct ModelGroupBuilder {
    name: String,
    db: Option<Database>,
    models: Vec<Registered>,
    auto_create_table: bool,
}

impl ModelGroupBuilder {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            db: None,
            models: Vec::new(),
            auto_create_table: true,
        }
    }

    /// Sets the connection the group's models share.
    #[must_use]
    pub fn db(mut self, db: Database) -> Self {
        self.db = Some(db);
        self
    }

    /// Adds a model to the group.
    #[must_use]
    pub fn model<M: Table>(mut self, model: M) -> Self {
        let shared = Arc::new(model);
        self.models.push(Registered {
            table: shared.clone() as Arc<dyn Table>,
            value: shared as Arc<dyn Any + Send + Sync>,
            type_id: TypeId::of::<M>(),
            name: std::any::type_name::<M>(),
        });
        self
    }

    /// Adds several models.
    #[must_use]
    pub fn models<I, M>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = M>,
        M: Table,
    {
        for model in models {
            self = self.model(model);
        }
        self
    }

    /// Whether tables are created at startup. On by default.
    #[must_use]
    pub fn auto_create_table(mut self, auto: bool) -> Self {
        self.auto_create_table = auto;
        self
    }

    /// Finishes the group.
    #[must_use]
    pub fn build(self) -> ModelGroup {
        ModelGroup {
            name: self.name,
            db: self.db,
            models: self.models,
        }
    }

    /// Whether tables will be created at startup.
    #[must_use]
    pub fn will_create_tables(&self) -> bool {
        self.auto_create_table
    }
}

/// A group plus its table-creation policy, as held by the application.
pub(crate) struct RegisteredGroup {
    pub group: ModelGroup,
    pub auto_create_table: bool,
}

impl RegisteredGroup {
    pub(crate) fn new(builder: ModelGroupBuilder) -> Self {
        let auto_create_table = builder.auto_create_table;
        Self {
            group: builder.build(),
            auto_create_table,
        }
    }

    /// Adds another model to an already-built group.
    pub(crate) fn add_model<M: Table>(&mut self, model: M) {
        self.group.add_model(model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::define_model;
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "t_widget")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    define_model!(WidgetModel, Entity, "t_widget");

    #[tokio::test]
    async fn a_group_prepares_and_registers_its_models() {
        let db = crate::db::connect_memory().await.unwrap();
        let group = ModelGroup::builder("widgets")
            .db(db)
            .model(WidgetModel::new(crate::db::connect_memory().await.unwrap()))
            .build();

        let ctx = Context::empty();
        group.init(&ctx, true).await.unwrap();

        let widgets = ctx.service::<WidgetModel>().unwrap();
        assert_eq!(widgets.table_name(), "t_widget");
        assert!(widgets.is_exist().await.unwrap());
        assert_eq!(group.model_names().len(), 1);
    }

    #[tokio::test]
    async fn auto_create_table_can_be_turned_off() {
        let builder = ModelGroup::builder("widgets")
            .model(WidgetModel::new(crate::db::connect_memory().await.unwrap()));
        assert!(builder.will_create_tables());

        let group = builder.auto_create_table(false).build();
        let ctx = Context::empty();
        group.init(&ctx, false).await.unwrap();
        // No table was created, so a query fails.
        assert!(ctx.service::<WidgetModel>().unwrap().count().await.is_err());
    }

    #[tokio::test]
    async fn transactions_require_a_database() {
        let group = ModelGroup::builder("empty").build();
        assert!(group.transaction().is_err());
    }
}
