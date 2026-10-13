//! Fluent, string-conditioned query builders.
//!
//! Conditions are SQL fragments with `?` placeholders — the same shape as the Go
//! original's `Where("age > ?", 18)`. Every backend understands `?`: sea-query rewrites
//! it to `$1` for PostgreSQL.

use std::marker::PhantomData;

use sea_orm::sea_query::{Alias, Expr, Order, Query as SeaQuery};
use sea_orm::{
    ConnectionTrait, EntityTrait, IdenStatic, Iterable, PrimaryKeyToColumn, QueryFilter,
    QuerySelect, QueryTrait, Select, Value,
};

use crate::db::Database;
use crate::util::{Page, Paged};
use crate::web::{Error, Result};

/// Builds a heterogeneous list of bound parameters:
///
/// ```no_run
/// # use rust_web_frame::model::args;
/// let values = args![18, "alice", true];
/// ```
#[macro_export]
macro_rules! args {
    () => { ::std::vec::Vec::<::sea_orm::Value>::new() };
    ($($value:expr),+ $(,)?) => {
        ::std::vec![$(::sea_orm::Value::from($value)),+]
    };
}

/// A condition plus its bound parameters.
pub(crate) type Condition = (String, Vec<Value>);

/// True when `name` is a plain (optionally table-qualified) column name, and can
/// therefore be quoted as an identifier rather than interpolated as raw SQL.
fn is_plain_column(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Turns a column name into an expression: quoted when it is a plain identifier, raw
/// otherwise (so `COUNT(*)` and `date(created_at)` still work).
fn column_expr(name: &str) -> Expr {
    if is_plain_column(name) {
        Expr::col(Alias::new(name))
    } else {
        Expr::cust(name.to_string())
    }
}

/// A read query for entity `E`.
///
/// ```no_run
/// # mod entity {
/// #     use sea_orm::entity::prelude::*;
/// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// #     #[sea_orm(table_name = "t_user")]
/// #     pub struct Model {
/// #         #[sea_orm(primary_key)]
/// #         pub id: i64,
/// #         pub age: i32,
/// #     }
/// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// #     pub enum Relation {}
/// #     impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # use rust_web_frame::model::Model;
/// # async fn demo(users: Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
/// let adults = users.query()
///     .filter("age > ?", rust_web_frame::args![18])
///     .order_by_desc("id")
///     .list(100)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Query<E: EntityTrait> {
    db: Database,
    conditions: Vec<Condition>,
    orders: Vec<(String, Order)>,
    groups: Vec<String>,
    having: Vec<Condition>,
    distinct: bool,
    select_exprs: Vec<String>,
    select_only: bool,
    limit: Option<u64>,
    offset: Option<u64>,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Query<E> {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            conditions: Vec::new(),
            orders: Vec::new(),
            groups: Vec::new(),
            having: Vec::new(),
            distinct: false,
            select_exprs: Vec::new(),
            select_only: false,
            limit: None,
            offset: None,
            _entity: PhantomData,
        }
    }

    /// The connection this query runs against.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Runs the query against another connection — a transaction, typically.
    pub fn with_db(mut self, db: Database) -> Self {
        self.db = db;
        self
    }

    /// Adds a `WHERE` condition.
    pub fn filter(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.conditions
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Adds a `WHERE` condition only when `value` is `Some` — the usual shape of an
    /// optional filter.
    pub fn filter_if(self, value: Option<Value>, condition: impl Into<String>) -> Self {
        match value {
            Some(value) => self.filter(condition, [value]),
            None => self,
        }
    }

    /// Adds `WHERE column = ?`.
    pub fn filter_eq(self, column: &str, value: impl Into<Value>) -> Self {
        self.filter(format!("{column} = ?"), [value.into()])
    }

    /// Adds `WHERE column IN (...)`.
    pub fn filter_in<V: Into<Value>>(
        mut self,
        column: &str,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values: Vec<Value> = values.into_iter().map(Into::into).collect();
        if values.is_empty() {
            // `IN ()` is invalid SQL; an empty set matches nothing.
            return self.filter("1 = 0", Vec::<Value>::new());
        }
        let placeholders = vec!["?"; values.len()].join(", ");
        self.conditions
            .push((format!("{column} IN ({placeholders})"), values));
        self
    }

    /// Adds an `ORDER BY` clause. `"id desc"`, `"name asc"` and bare column names work.
    pub fn order(mut self, clause: impl Into<String>) -> Self {
        let clause = clause.into();
        let (column, order) = match clause.rsplit_once(' ') {
            Some((column, dir)) if dir.eq_ignore_ascii_case("desc") => (column, Order::Desc),
            Some((column, dir)) if dir.eq_ignore_ascii_case("asc") => (column, Order::Asc),
            _ => (clause.as_str(), Order::Asc),
        };
        self.orders.push((column.trim().to_string(), order));
        self
    }

    /// Adds `ORDER BY column DESC`.
    pub fn order_by_desc(self, column: &str) -> Self {
        self.order(format!("{column} desc"))
    }

    /// Adds `ORDER BY column ASC`.
    pub fn order_by_asc(self, column: &str) -> Self {
        self.order(format!("{column} asc"))
    }

    /// Adds a `GROUP BY` clause.
    pub fn group_by(mut self, column: impl Into<String>) -> Self {
        self.groups.push(column.into());
        self
    }

    /// Adds a `HAVING` clause.
    pub fn having(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.having
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Selects a raw expression instead of the entity's columns, e.g. `"city, COUNT(*) AS total"`.
    pub fn select_expr(mut self, expression: impl Into<String>) -> Self {
        self.select_only = true;
        self.select_exprs.push(expression.into());
        self
    }

    /// Adds `DISTINCT`.
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Caps the number of rows returned.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Skips the first `offset` rows.
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Builds the sea-orm select statement this query describes.
    fn build(&self) -> Select<E> {
        let mut select = E::find();

        if self.select_only {
            select = select.select_only();
            for expression in &self.select_exprs {
                select = select.expr(Expr::cust(expression.clone()));
            }
        }

        for (condition, values) in &self.conditions {
            select = select.filter(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        for (column, order) in &self.orders {
            QueryTrait::query(&mut select).order_by_expr(column_expr(column), order.clone());
        }
        for group in &self.groups {
            QueryTrait::query(&mut select).group_by_col(Alias::new(group.as_str()));
        }
        for (condition, values) in &self.having {
            QueryTrait::query(&mut select)
                .and_having(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        if self.distinct {
            QueryTrait::query(&mut select).distinct();
        }
        if let Some(limit) = self.limit {
            select = select.limit(limit);
        }
        if let Some(offset) = self.offset {
            select = select.offset(offset);
        }
        select
    }

    /// Runs the query and returns every matching row.
    pub async fn all(self) -> Result<Vec<E::Model>> {
        self.build().all(&self.db).await.map_err(Error::from)
    }

    /// Runs the query and returns the first matching row, or `None`.
    ///
    /// A missing row is `None` rather than an error, so the "did I find anything?"
    /// question is answered by the type even though the Go original answers it with a
    /// zero value.
    pub async fn one(self) -> Result<Option<E::Model>> {
        let db = self.db.clone();
        self.limit(1).build().one(&db).await.map_err(Error::from)
    }

    /// Returns the first matching row, failing with `404` when there is none.
    pub async fn one_required(self) -> Result<E::Model> {
        self.one()
            .await?
            .ok_or_else(|| Error::not_found().detail("no matching record"))
    }

    /// Counts the matching rows.
    pub async fn count(self) -> Result<i64> {
        let db = self.db.clone();
        count_rows(self.build(), &db).await
    }

    /// Reports whether any row matches.
    pub async fn exists(self) -> Result<bool> {
        let db = self.db.clone();
        Ok(self
            .limit(1)
            .build()
            .one(&db)
            .await
            .map_err(Error::from)?
            .is_some())
    }

    /// Returns at most `size` rows.
    pub async fn list(self, size: u64) -> Result<Vec<E::Model>> {
        self.limit(size).all().await
    }

    /// Returns one page of rows plus the total row count.
    pub async fn page(self, page: &Page) -> Result<(Vec<E::Model>, i64)> {
        let page = page.with_defaults();
        let db = self.db.clone();
        let total = count_rows(self.build(), &db).await?;
        let rows = self
            .limit(page.page_size as u64)
            .offset(((page.page_no - 1) * page.page_size) as u64)
            .build()
            .all(&db)
            .await
            .map_err(Error::from)?;
        Ok((rows, total))
    }

    /// Returns one page of rows wrapped in a [`Paged`], ready to be returned from a
    /// handler.
    pub async fn paged(self, page: &Page) -> Result<Paged<E::Model>> {
        let (list, total) = self.page(page).await?;
        Ok(Paged::new(total, list))
    }

    /// Reads a projection into `T`.
    ///
    /// ```no_run
    /// # use rust_web_frame::model::Model;
    /// #[derive(Debug, sea_orm::FromQueryResult)]
    /// struct CityStat { city: String, total: i64 }
    ///
    /// # mod entity {
    /// #     use sea_orm::entity::prelude::*;
    /// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// #     #[sea_orm(table_name = "t_user")]
    /// #     pub struct Model {
    /// #         #[sea_orm(primary_key)]
    /// #         pub id: i64,
    /// #         pub age: i32,
    /// #     }
    /// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    /// #     pub enum Relation {}
    /// #     impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(m: Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
    /// let stats = m.query()
    ///     .select_expr("city, COUNT(*) AS total")
    ///     .group_by("city")
    ///     .into_models::<CityStat>()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn into_models<T>(self) -> Result<Vec<T>>
    where
        T: sea_orm::FromQueryResult + Send + Sync,
    {
        self.build()
            .into_model::<T>()
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    /// Runs raw SQL, appending the accumulated `WHERE` conditions.
    pub async fn exec(self, sql: impl Into<String>) -> Result<Vec<E::Model>> {
        let mut statement = sql.into();
        let mut values: Vec<Value> = Vec::new();
        for (condition, args) in &self.conditions {
            statement.push_str(" AND ");
            statement.push_str(condition);
            values.extend(args.iter().cloned());
        }
        E::find()
            .from_raw_sql(sea_orm::Statement::from_sql_and_values(
                self.db.get_database_backend(),
                statement,
                values,
            ))
            .all(&self.db)
            .await
            .map_err(Error::from)
    }
}

/// An `UPDATE` statement addressed by string column names.
pub struct Update<E: EntityTrait> {
    db: Database,
    table: String,
    assignments: Vec<(String, Value)>,
    conditions: Vec<Condition>,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Update<E> {
    pub(crate) fn new(db: Database, table: impl Into<String>) -> Self {
        Self {
            db,
            table: table.into(),
            assignments: Vec::new(),
            conditions: Vec::new(),
            _entity: PhantomData,
        }
    }

    /// Sets one column.
    pub fn set(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.assignments.push((column.into(), value.into()));
        self
    }

    /// Sets several columns.
    pub fn set_map<K: Into<String>>(
        mut self,
        values: impl IntoIterator<Item = (K, Value)>,
    ) -> Self {
        self.assignments
            .extend(values.into_iter().map(|(k, v)| (k.into(), v)));
        self
    }

    /// Restricts the update with a `WHERE` condition.
    pub fn filter(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.conditions
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Restricts the update to a primary key.
    pub fn by_pk(self, value: impl Into<Value>) -> Self {
        let column = primary_key_name::<E>();
        self.filter(format!("{column} = ?"), [value.into()])
    }

    /// Runs the update against another connection.
    pub fn with_db(mut self, db: Database) -> Self {
        self.db = db;
        self
    }

    /// Reports whether any column was set. An update with no `SET` clause is a no-op.
    pub fn has_assignments(&self) -> bool {
        !self.assignments.is_empty()
    }

    /// Executes the update, returning how many rows changed.
    pub async fn execute(self) -> Result<u64> {
        if self.assignments.is_empty() {
            return Ok(0);
        }
        let backend = self.db.get_database_backend();
        let mut query = SeaQuery::update();
        query.table(Alias::new(self.table.as_str()));
        for (column, value) in &self.assignments {
            query.value(Alias::new(column.as_str()), Expr::value(value.clone()));
        }
        for (condition, values) in &self.conditions {
            query.and_where(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        let result = self
            .db
            .execute_raw(backend.build(&query))
            .await
            .map_err(Error::from)?;
        Ok(result.rows_affected())
    }
}

/// A `DELETE` statement addressed by string conditions.
pub struct Delete<E: EntityTrait> {
    db: Database,
    table: String,
    conditions: Vec<Condition>,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Delete<E> {
    pub(crate) fn new(db: Database, table: impl Into<String>) -> Self {
        Self {
            db,
            table: table.into(),
            conditions: Vec::new(),
            _entity: PhantomData,
        }
    }

    /// Restricts the delete with a `WHERE` condition.
    pub fn filter(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.conditions
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Restricts the delete to a primary key.
    pub fn by_pk(self, value: impl Into<Value>) -> Self {
        let column = primary_key_name::<E>();
        self.filter(format!("{column} = ?"), [value.into()])
    }

    /// Runs the delete against another connection.
    pub fn with_db(mut self, db: Database) -> Self {
        self.db = db;
        self
    }

    /// Executes the delete, returning how many rows were removed.
    pub async fn execute(self) -> Result<u64> {
        let backend = self.db.get_database_backend();
        let mut query = SeaQuery::delete();
        query.from_table(Alias::new(self.table.as_str()));
        for (condition, values) in &self.conditions {
            query.and_where(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        let result = self
            .db
            .execute_raw(backend.build(&query))
            .await
            .map_err(Error::from)?;
        Ok(result.rows_affected())
    }
}

/// An aggregate query: `SUM`, `COUNT`, `AVG`, `GROUP BY`, `HAVING`, `DISTINCT`.
///
/// ```no_run
/// # use rust_web_frame::model::Model;
/// #[derive(Debug, sea_orm::FromQueryResult)]
/// struct CityStat { city: String, total: i64 }
///
/// # mod entity {
/// #     use sea_orm::entity::prelude::*;
/// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// #     #[sea_orm(table_name = "t_user")]
/// #     pub struct Model {
/// #         #[sea_orm(primary_key)]
/// #         pub id: i64,
/// #         pub age: i32,
/// #     }
/// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
/// #     pub enum Relation {}
/// #     impl ActiveModelBehavior for ActiveModel {}
/// # }
/// # async fn demo(m: Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
/// let stats = m.aggregate()
///     .select("city, SUM(amount) AS total")
///     .group_by("city")
///     .having("SUM(amount) > ?", rust_web_frame::args![200])
///     .rows::<CityStat>()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Aggregate<E: EntityTrait> {
    db: Database,
    expressions: Vec<String>,
    conditions: Vec<Condition>,
    groups: Vec<String>,
    having: Vec<Condition>,
    distinct: bool,
    limit: Option<u64>,
    _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> Aggregate<E> {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            expressions: Vec::new(),
            conditions: Vec::new(),
            groups: Vec::new(),
            having: Vec::new(),
            distinct: false,
            limit: None,
            _entity: PhantomData,
        }
    }

    /// Adds a `SELECT` expression, e.g. `"COUNT(*)"` or `"city, SUM(amount) AS total"`.
    pub fn select(mut self, expression: impl Into<String>) -> Self {
        self.expressions.push(expression.into());
        self
    }

    /// Adds a `WHERE` condition.
    pub fn filter(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.conditions
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Adds a `GROUP BY` clause.
    pub fn group_by(mut self, column: impl Into<String>) -> Self {
        self.groups.push(column.into());
        self
    }

    /// Adds a `HAVING` clause.
    pub fn having(
        mut self,
        condition: impl Into<String>,
        values: impl IntoIterator<Item = Value>,
    ) -> Self {
        self.having
            .push((condition.into(), values.into_iter().collect()));
        self
    }

    /// Adds `DISTINCT`.
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Caps the number of grouped rows.
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Runs the aggregate against another connection.
    pub fn with_db(mut self, db: Database) -> Self {
        self.db = db;
        self
    }

    fn build(&self) -> Select<E> {
        let mut select = E::find().select_only();
        if self.expressions.is_empty() {
            select = select.expr(Expr::cust("*"));
        }
        for expression in &self.expressions {
            select = select.expr(Expr::cust(expression.clone()));
        }
        for (condition, values) in &self.conditions {
            select = select.filter(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        for group in &self.groups {
            QueryTrait::query(&mut select).group_by_col(Alias::new(group.as_str()));
        }
        for (condition, values) in &self.having {
            QueryTrait::query(&mut select)
                .and_having(Expr::cust_with_values(condition.clone(), values.clone()));
        }
        if self.distinct {
            QueryTrait::query(&mut select).distinct();
        }
        if let Some(limit) = self.limit {
            select = select.limit(limit);
        }
        select
    }

    /// Runs the aggregate and reads every row into `T`.
    pub async fn rows<T>(self) -> Result<Vec<T>>
    where
        T: sea_orm::FromQueryResult + Send + Sync,
    {
        self.build()
            .into_model::<T>()
            .all(&self.db)
            .await
            .map_err(Error::from)
    }

    /// Runs the aggregate and reads the first row into `T`.
    pub async fn row<T>(self) -> Result<Option<T>>
    where
        T: sea_orm::FromQueryResult + Send + Sync,
    {
        Ok(self.rows::<T>().await?.into_iter().next())
    }

    /// Runs the aggregate and reads a single scalar, e.g. `SUM(amount)` or `COUNT(*)`.
    ///
    /// ```no_run
    /// # use rust_web_frame::model::Model;
    /// # mod entity {
    /// #     use sea_orm::entity::prelude::*;
    /// #     #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    /// #     #[sea_orm(table_name = "t_user")]
    /// #     pub struct Model {
    /// #         #[sea_orm(primary_key)]
    /// #         pub id: i64,
    /// #         pub age: i32,
    /// #     }
    /// #     #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    /// #     pub enum Relation {}
    /// #     impl ActiveModelBehavior for ActiveModel {}
    /// # }
    /// # async fn demo(orders: Model<entity::Entity>) -> Result<(), rust_web_frame::web::Error> {
    /// let total: Option<f64> = orders.aggregate().select("SUM(amount)").value().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn value<T>(self) -> Result<Option<T>>
    where
        T: sea_orm::TryGetable + Send + Sync + 'static,
    {
        self.build()
            .into_tuple::<T>()
            .one(&self.db)
            .await
            .map_err(Error::from)
    }
}

/// Counts the rows a select statement yields, by wrapping it in a `COUNT(*)` subquery.
///
/// Rolled by hand rather than through [`sea_orm::Paginator`], whose `count` requires the
/// entity's model type to be `Sync` — a bound the generic layer would have to push onto
/// every caller.
async fn count_rows<E: EntityTrait>(select: Select<E>, db: &Database) -> Result<i64> {
    use sea_orm::sea_query::{Alias, Expr as SeaExpr, SelectStatement};

    let backend = db.get_database_backend();
    let mut inner = QueryTrait::into_query(select);
    inner.clear_order_by();
    let statement = SelectStatement::new()
        .expr(SeaExpr::cust("COUNT(*) AS num_items"))
        .from_subquery(inner, Alias::new("sub_query"))
        .to_owned();

    match db
        .query_one_raw(backend.build(&statement))
        .await
        .map_err(Error::from)?
    {
        Some(row) => row.try_get::<i64>("", "num_items").map_err(Error::from),
        None => Ok(0),
    }
}

/// The primary-key column name of an entity.
pub(crate) fn primary_key_name<E: EntityTrait>() -> String {
    let primary_key = E::PrimaryKey::iter()
        .next()
        .expect("every entity has at least one primary key column");
    primary_key.into_column().as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_columns_are_recognised() {
        assert!(is_plain_column("id"));
        assert!(is_plain_column("t_user.id"));
        assert!(!is_plain_column("COUNT(*)"));
        assert!(!is_plain_column("id; DROP TABLE users"));
        assert!(!is_plain_column(""));
    }

    #[test]
    fn args_macro_builds_heterogeneous_bindings() {
        let values = crate::args![18, "alice", true];
        assert_eq!(values.len(), 3);
        assert!(crate::args![].is_empty());
    }
}
