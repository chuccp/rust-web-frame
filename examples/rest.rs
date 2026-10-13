//! A REST controller with a service layer behind it.
//!
//! ```text
//! cargo run --example rest
//! curl http://localhost:19009/api/users
//! curl -X POST http://localhost:19009/api/users -H "content-type: application/json" \
//!      -d '{"name":"carol","email":"carol@example.com"}'
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use async_trait::async_trait;
use rust_web_frame as wf;
use serde::{Deserialize, Serialize};
use serde_json::json;
use wf::config::load_auto_config;
use wf::core::{Context, Rest, Service};
use wf::web::{Error, Message, Page, Paged, Request, Response, Result, handler};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

/// Business logic, kept out of the controller.
#[derive(Default)]
pub struct UserService {
    users: RwLock<Vec<User>>,
    next_id: AtomicU64,
}

#[async_trait]
impl Service for UserService {}

impl UserService {
    pub fn create(&self, name: String, email: String) -> Result<User> {
        let mut users = self.users.write().expect("users lock poisoned");
        if users
            .iter()
            .any(|user| user.email.eq_ignore_ascii_case(&email))
        {
            return Err(Error::duplicate_entry().detail(format!("{email} is already registered")));
        }
        let user = User {
            id: self.next_id.fetch_add(1, Ordering::SeqCst) + 1,
            name,
            email,
        };
        users.push(user.clone());
        Ok(user)
    }

    #[must_use]
    pub fn list(&self, page: &Page) -> Paged<User> {
        let users = self.users.read().expect("users lock poisoned");
        let page = page.with_defaults();
        let offset = ((page.page_no - 1) * page.page_size).max(0) as usize;
        let list = users
            .iter()
            .skip(offset)
            .take(page.page_size as usize)
            .cloned()
            .collect();
        Paged::new(users.len() as i64, list)
    }

    pub fn find(&self, id: u64) -> Result<User> {
        self.users
            .read()
            .expect("users lock poisoned")
            .iter()
            .find(|user| user.id == id)
            .cloned()
            .ok_or_else(|| Error::not_found().detail(format!("user {id} does not exist")))
    }

    pub fn delete(&self, id: u64) -> Result<()> {
        let mut users = self.users.write().expect("users lock poisoned");
        let before = users.len();
        users.retain(|user| user.id != id);
        if users.len() == before {
            return Err(Error::not_found().detail(format!("user {id} does not exist")));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct CreateUser {
    name: String,
    email: String,
}

/// A controller looks its dependencies up once, at startup.
#[derive(Default)]
pub struct UserController {
    users: OnceLock<std::sync::Arc<UserService>>,
}

impl UserController {
    async fn list(req: Request) -> Result<Message> {
        let service = Self::service(&req);
        Ok(Message::data(json!(service.list(&req.page()?))))
    }

    async fn show(req: Request) -> Result<Message> {
        let service = Self::service(&req);
        Ok(Message::data(json!(service.find(req.param_u64("id"))?)))
    }

    async fn create(req: Request) -> Result<Response> {
        let payload: CreateUser = req.bind_json()?;
        let service = Self::service(&req);
        let user = service.create(payload.name, payload.email)?;
        Ok(Response::json(json!(user)).with_status(201))
    }

    async fn delete(req: Request) -> Result<Message> {
        let service = Self::service(&req);
        service.delete(req.param_u64("id"))?;
        Ok(Message::with_message("deleted"))
    }

    fn service(req: &Request) -> std::sync::Arc<UserService> {
        req.context()
            .service::<UserService>()
            .expect("UserService is registered before the controller")
    }
}

#[async_trait]
impl Rest for UserController {
    async fn init(&self, ctx: &Context) -> Result<()> {
        self.users
            .set(ctx.service::<UserService>()?)
            .map_err(|_| Error::internal().detail("UserController was initialised twice"))?;

        ctx.get("/api/users", handler(Self::list));
        ctx.get("/api/users/:id", handler(Self::show));
        ctx.post("/api/users", handler(Self::create));
        ctx.delete("/api/users/:id", handler(Self::delete));
        ctx.get("/healthz", handler(health));
        Ok(())
    }
}

async fn health(_req: Request) -> Result<Response> {
    Ok(Response::json(json!({ "status": "ok" })))
}

/// A background task that keeps running until the application stops.
#[derive(Default)]
struct Reporter;

#[async_trait]
impl wf::core::Runner for Reporter {
    async fn run(&self, ctx: &Context) -> Result<()> {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        let cancel = ctx.cancellation_token();
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let users = ctx.service::<UserService>().expect("registered");
                    tracing::info!(count = users.list(&Page::new(1, 1)).total, "users so far");
                }
                _ = cancel.cancelled() => return Ok(()),
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);

    // Dependencies first: the service is registered before the controller that needs it.
    builder.service(UserService::default());
    builder.rest(UserController::default());
    builder.runner(Reporter);

    // The framework's own health endpoint, alongside the hand-written one above.
    builder.get(
        "/readyz",
        handler(|_req| async { Ok(Response::json(json!({ "ready": true }))) }),
    );

    builder.build().run().await?;
    Ok(())
}
