//! Transaction semantics over a real database.
//!
//! The framework's job here is narrow and easy to get subtly wrong: hand the body a handle
//! that *is* the transaction, commit when it returns `Ok`, roll back when it returns `Err`,
//! and make nesting work. These tests check each of those against SQLite rather than against
//! the intent.

use std::sync::Arc;
use std::time::Duration;

use rust_web_frame::db::{Database, DbConfig, connect, transaction};
use rust_web_frame::model::Model;
use rust_web_frame::web::{Error, Result};
use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::{ColumnTrait, QueryFilter, Set};

pub mod account {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "t_account")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub name: String,
        pub balance: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

async fn setup() -> Model<account::Entity> {
    let db = connect(&DbConfig::sqlite_memory()).await.unwrap();
    let accounts = Model::<account::Entity>::new(db);
    accounts.create_table().await.unwrap();
    for (name, balance) in [("alice", 100), ("bob", 50)] {
        account::Entity::insert(account::ActiveModel {
            name: Set(name.to_owned()),
            balance: Set(balance),
            ..Default::default()
        })
        .exec(accounts.db())
        .await
        .unwrap();
    }
    accounts
}

async fn balance(model: &Model<account::Entity>, id: i64) -> i64 {
    account::Entity::find_by_id(id)
        .one(model.db())
        .await
        .unwrap()
        .expect("the account exists")
        .balance
}

async fn set_balance(model: &Model<account::Entity>, id: i64, value: i64) -> Result<()> {
    account::Entity::update_many()
        .col_expr(account::Column::Balance, Expr::value(value))
        .filter(account::Column::Id.eq(id))
        .exec(model.db())
        .await?;
    Ok(())
}

/// `SET balance = balance + delta`, so a concurrent writer cannot lose an update the way a
/// read-modify-write would.
async fn add_balance(model: &Model<account::Entity>, id: i64, delta: i64) -> Result<()> {
    account::Entity::update_many()
        .col_expr(
            account::Column::Balance,
            Expr::col(account::Column::Balance).add(delta),
        )
        .filter(account::Column::Id.eq(id))
        .exec(model.db())
        .await?;
    Ok(())
}

// ------------------------------------------------------------------ basics

#[tokio::test]
async fn a_committed_transaction_is_visible_afterwards() {
    let accounts = setup().await;

    accounts
        .db()
        .transaction(|tx| {
            let inside = accounts.with_db(tx);
            async move {
                set_balance(&inside, 1, 999).await?;
                // The change is visible *inside* the transaction.
                assert_eq!(balance(&inside, 1).await, 999);
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(balance(&accounts, 1).await, 999);
}

#[tokio::test]
async fn a_failed_transaction_changes_nothing() {
    let accounts = setup().await;

    let outcome: Result<()> = accounts
        .db()
        .transaction(|tx| {
            let inside = accounts.with_db(tx);
            async move {
                set_balance(&inside, 1, 0).await?;
                set_balance(&inside, 2, 0).await?;
                Err(Error::internal().detail("changed my mind"))
            }
        })
        .await;

    assert!(outcome.is_err());
    assert_eq!(
        balance(&accounts, 1).await,
        100,
        "the first write must be undone"
    );
    assert_eq!(balance(&accounts, 2).await, 50, "so must the second");
}

#[tokio::test]
async fn an_uncommitted_write_is_not_visible_to_another_connection() {
    // A file-backed database, so another connection really is another connection. Through
    // an in-memory one every connection would be a separate database and the question would
    // be about the pool rather than about isolation.
    let dir = tempfile::tempdir().unwrap();
    let db = connect(&DbConfig::sqlite(dir.path().join("a.db").to_string_lossy()))
        .await
        .unwrap();
    let accounts = Model::<account::Entity>::new(db.clone());
    accounts.create_table().await.unwrap();
    account::Entity::insert(account::ActiveModel {
        name: Set("alice".to_owned()),
        balance: Set(100),
        ..Default::default()
    })
    .exec(accounts.db())
    .await
    .unwrap();

    // A second connection, as a different request would have.
    let other = connect(&DbConfig::sqlite(dir.path().join("a.db").to_string_lossy()))
        .await
        .unwrap();
    let outside = Model::<account::Entity>::new(other);

    db.transaction(|tx| {
        let inside = accounts.with_db(tx);
        let outside = outside.clone();
        async move {
            set_balance(&inside, 1, 7).await?;
            assert_eq!(
                balance(&inside, 1).await,
                7,
                "the transaction sees its own write"
            );
            assert_eq!(
                balance(&outside, 1).await,
                100,
                "another connection sees the committed value, not the uncommitted one"
            );
            Ok(())
        }
    })
    .await
    .unwrap();

    assert_eq!(balance(&accounts, 1).await, 7);
}

// ----------------------------------------------------------------- nesting

#[tokio::test]
async fn a_nested_transaction_rolls_back_only_its_own_work() {
    let accounts = setup().await;

    accounts
        .db()
        .transaction(|outer| {
            let outer_accounts = accounts.with_db(outer);
            async move {
                set_balance(&outer_accounts, 1, 500).await?;

                // The inner scope fails; the outer one carries on.
                let inner: Result<()> = outer_accounts
                    .db()
                    .transaction(|inner| {
                        let inside = outer_accounts.with_db(inner);
                        async move {
                            set_balance(&inside, 2, 1_000).await?;
                            Err(Error::internal().detail("inner failed"))
                        }
                    })
                    .await;
                assert!(inner.is_err());

                // The outer transaction still sees its own write and not the inner one.
                assert_eq!(balance(&outer_accounts, 1).await, 500);
                assert_eq!(
                    balance(&outer_accounts, 2).await,
                    50,
                    "the inner write is gone"
                );
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        balance(&accounts, 1).await,
        500,
        "the outer write committed"
    );
    assert_eq!(balance(&accounts, 2).await, 50, "the inner one did not");
}

#[tokio::test]
async fn a_nested_transaction_that_succeeds_commits_with_its_parent() {
    let accounts = setup().await;

    accounts
        .db()
        .transaction(|outer| {
            let outer_accounts = accounts.with_db(outer);
            async move {
                outer_accounts
                    .db()
                    .transaction(|inner| {
                        let inside = outer_accounts.with_db(inner);
                        async move {
                            set_balance(&inside, 1, 111).await?;
                            Ok(())
                        }
                    })
                    .await?;
                set_balance(&outer_accounts, 2, 222).await?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(balance(&accounts, 1).await, 111);
    assert_eq!(balance(&accounts, 2).await, 222);
}

#[tokio::test]
async fn an_outer_rollback_takes_the_inner_commit_with_it() {
    let accounts = setup().await;

    let outcome: Result<()> = accounts
        .db()
        .transaction(|outer| {
            let outer_accounts = accounts.with_db(outer);
            async move {
                outer_accounts
                    .db()
                    .transaction(|inner| {
                        let inside = outer_accounts.with_db(inner);
                        async move {
                            set_balance(&inside, 1, 777).await?;
                            Ok(())
                        }
                    })
                    .await?;

                // The inner transaction committed, and the outer one then gives up.
                Err(Error::internal().detail("outer failed after the inner one"))
            }
        })
        .await;

    assert!(outcome.is_err());
    assert_eq!(
        balance(&accounts, 1).await,
        100,
        "a savepoint that was released is still undone by its parent"
    );
}

// ------------------------------------------------------------- the edges

#[tokio::test]
async fn a_handle_that_escapes_stops_the_commit_from_being_silent() {
    let accounts = setup().await;
    let escaped: Arc<std::sync::Mutex<Option<Database>>> = Arc::new(std::sync::Mutex::new(None));

    let leak = Arc::clone(&escaped);
    let outcome = accounts
        .db()
        .transaction(|tx| {
            let inside = accounts.with_db(tx);
            let leak = Arc::clone(&leak);
            async move {
                set_balance(&inside, 1, 42).await?;
                // Keeping a handle past the end of the transaction — into a task, say.
                *leak.lock().unwrap() = Some(inside.db().clone());
                Ok(())
            }
        })
        .await;

    assert!(
        outcome.is_err(),
        "a transaction whose handle escaped cannot be committed, and must say so"
    );
    let message = outcome.unwrap_err().client_message();
    assert!(
        message.contains("outlived"),
        "the message should explain why: {message}"
    );

    // Releasing the clone is what lets the transaction roll back and the connection go back
    // to the pool; until then it is held, which is the cost of the mistake.
    *escaped.lock().unwrap() = None;

    let recovered = tokio::time::timeout(Duration::from_secs(10), balance(&accounts, 1)).await;
    assert!(
        recovered.is_ok(),
        "the connection must come back once the clone is gone"
    );
    assert_eq!(
        recovered.unwrap(),
        100,
        "the write was rolled back, not half-applied"
    );
}

#[tokio::test]
async fn a_panic_in_the_body_leaves_the_database_alone() {
    let accounts = setup().await;
    let inside = accounts.clone();

    let outcome = tokio::spawn(async move {
        let result: Result<()> = inside
            .db()
            .transaction(|tx| {
                let scoped = inside.with_db(tx);
                async move {
                    set_balance(&scoped, 1, 1).await?;
                    panic!("the body blew up");
                }
            })
            .await;
        result
    })
    .await;

    assert!(outcome.is_err(), "the task panicked");

    // The transaction was dropped without a commit, and sea-orm rolls back on drop.
    assert_eq!(balance(&accounts, 1).await, 100);
}

#[tokio::test]
async fn the_free_function_does_the_same_as_the_method() {
    let accounts = setup().await;

    transaction(&accounts.db().clone(), |tx| {
        let inside = accounts.with_db(tx);
        async move {
            set_balance(&inside, 1, 55).await?;
            Ok(())
        }
    })
    .await
    .unwrap();

    assert_eq!(balance(&accounts, 1).await, 55);
}

// --------------------------------------------------------- through a model

#[tokio::test]
async fn a_transfer_is_atomic_across_two_models() {
    let accounts = setup().await;

    let transfer = |from: i64, to: i64, amount: i64| {
        let accounts = accounts.clone();
        async move {
            accounts
                .db()
                .transaction(|tx| {
                    let inside = accounts.with_db(tx);
                    async move {
                        let payer = account::Entity::find_by_id(from)
                            .one(inside.db())
                            .await?
                            .ok_or_else(|| Error::not_found().detail("no such payer"))?;
                        if payer.balance < amount {
                            return Err(Error::validation_failed().detail("insufficient funds"));
                        }
                        add_balance(&inside, from, -amount).await?;
                        add_balance(&inside, to, amount).await?;
                        Ok(())
                    }
                })
                .await
        }
    };

    // Not enough money: nothing moves.
    let refused = transfer(2, 1, 1_000).await;
    assert!(refused.is_err());
    assert_eq!(balance(&accounts, 1).await, 100);
    assert_eq!(balance(&accounts, 2).await, 50);

    // Enough: both sides move together.
    transfer(1, 2, 30).await.unwrap();
    assert_eq!(balance(&accounts, 1).await, 70);
    assert_eq!(balance(&accounts, 2).await, 80);
}

#[tokio::test]
async fn concurrent_transfers_do_not_lose_updates() {
    // A file-backed database, because an in-memory SQLite pool gives each connection its
    // own database and the test would then be measuring the pool, not the transactions.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("accounts.db");
    let db = connect(&DbConfig::sqlite(file.to_string_lossy()))
        .await
        .unwrap();

    let model = Model::<account::Entity>::new(db.clone());
    model.create_table().await.unwrap();
    account::Entity::insert(account::ActiveModel {
        name: Set("alice".to_owned()),
        balance: Set(0),
        ..Default::default()
    })
    .exec(model.db())
    .await
    .unwrap();

    // Twenty transactions each adding one, run concurrently.
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let model = model.clone();
        tasks.push(tokio::spawn(async move {
            model
                .db()
                .transaction(|tx| {
                    let inside = model.with_db(tx);
                    async move {
                        add_balance(&inside, 1, 1).await?;
                        Ok(())
                    }
                })
                .await
        }));
    }

    let mut failures = 0;
    for task in tasks {
        if task.await.unwrap().is_err() {
            // SQLite serialises writers and reports `database is locked` rather than
            // queueing; a busy timeout is the deployment's business, not the framework's.
            failures += 1;
        }
    }

    let final_balance = balance(&model, 1).await;
    assert_eq!(
        final_balance,
        20 - failures,
        "each successful transaction must have added exactly one, with none lost"
    );
    assert!(
        final_balance > 0,
        "at least some transactions should have succeeded"
    );
}

#[tokio::test]
async fn a_model_renewed_onto_a_transaction_uses_it() {
    let accounts = setup().await;
    let db = accounts.db().clone();

    // `Model::with_db` and `Database::transaction` have to agree about what a handle is.
    db.transaction(|tx| {
        let scoped = accounts.with_db(tx);
        async move {
            assert!(
                scoped.db().is_transaction(),
                "the model is bound to the transaction"
            );
            Ok(())
        }
    })
    .await
    .unwrap();

    assert!(!db.is_transaction(), "the pool is still a pool");
}

#[tokio::test]
async fn the_connection_reports_which_it_is() {
    let accounts = setup().await;
    let pool = accounts.db().clone();
    assert!(!pool.is_transaction());

    let also_pool = pool.clone();
    pool.transaction(|tx| async move {
        assert!(tx.is_transaction());
        // The pool is still usable inside the transaction, for reads that do not need it.
        assert!(!also_pool.is_transaction());
        Ok(())
    })
    .await
    .unwrap();
}
