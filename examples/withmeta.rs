//! Declarative routing: the route declares what it is, one filter enforces it.
//!
//! ```text
//! cargo run --example withmeta
//!
//! curl http://localhost:19009/api/login                       # public
//! curl http://localhost:19009/api/profile                      # 401
//! curl -H "authorization: alice"  http://localhost:19009/api/profile   # ok
//! curl -H "authorization: alice"  http://localhost:19009/api/admin/users  # 403
//! curl -H "authorization: root"   http://localhost:19009/api/admin/users  # ok
//! ```
//!
//! The `/api/admin` routes are also **guarded** — see `require_meta` in the controller's
//! `init`. It changes nothing while every route complies, which is the point: it is what
//! makes the *next* admin route safe, because one registered without `require_auth()`
//! refuses to start instead of serving unprotected.

use async_trait::async_trait;
use rust_web_frame as wf;
use rust_web_frame::config::load_auto_config;
use rust_web_frame::core::{Context, Rest};
use rust_web_frame::web::{
    Error, Filter, FilterChain, Message, MetaOption, Request, Response, Result, handler,
};
use serde_json::json;

// -------------------------------------------------------------- the metadata
//
// Each factory lives next to the filter that consumes it.

/// The route needs a valid token.
fn require_auth() -> MetaOption {
    MetaOption::value("require_auth", true)
}

/// The route is reachable without a token.
fn skip_auth() -> MetaOption {
    MetaOption::value("skip_auth", true)
}

/// The token must carry this permission.
fn require_permission(permission: &str) -> MetaOption {
    MetaOption::value("require_permission", permission)
}

// ------------------------------------------------------------------ the filter

/// One filter, three questions, one place.
#[derive(Default)]
struct AuthFilter;

#[async_trait]
impl Filter for AuthFilter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        // Did this route ask for authentication? Most routes did not.
        if !req.has_meta(require_auth()) || req.has_meta(skip_auth()) {
            return chain.next(req).await;
        }

        let Some(token) = req.header("authorization") else {
            return Err(Error::unauthorized().detail("missing Authorization header"));
        };

        let claims = verify(token)?;
        req.set_attr("user", claims.name.clone());

        // A route may name the permission it needs; `has_meta` only matches on the exact
        // value, so this asks "is *this* route tagged with *this* permission?".
        if req.has_meta(require_permission("admin:create_user")) && !claims.is_admin {
            return Err(Error::forbidden().detail("admin:create_user is required"));
        }
        if req.has_meta(require_permission("admin:delete_user")) && !claims.is_admin {
            return Err(Error::forbidden().detail("admin:delete_user is required"));
        }

        chain.next(req).await
    }
}

#[derive(Debug, Clone)]
struct Claims {
    name: String,
    is_admin: bool,
}

/// A stand-in for real token verification.
fn verify(token: &str) -> Result<Claims> {
    match token {
        "alice" => Ok(Claims {
            name: String::from("alice"),
            is_admin: false,
        }),
        "root" => Ok(Claims {
            name: String::from("root"),
            is_admin: true,
        }),
        other => Err(Error::token_invalid().detail(format!("unknown token `{other}`"))),
    }
}

// --------------------------------------------------------------- the controller

#[derive(Default)]
struct ApiController;

impl ApiController {
    async fn login(_req: Request) -> Result<Response> {
        Ok(Response::json(json!({ "token": "alice" })))
    }

    async fn profile(req: Request) -> Result<Message> {
        let name = req
            .attr::<String>("user")
            .map(|name| name.to_string())
            .unwrap_or_default();
        Ok(Message::data(json!({ "name": name })))
    }

    async fn create_user(req: Request) -> Result<Message> {
        let name = req
            .attr::<String>("user")
            .map(|name| name.to_string())
            .unwrap_or_default();
        Ok(Message::data(json!({ "createdBy": name })))
    }

    async fn delete_user(_req: Request) -> Result<Message> {
        Ok(Message::with_message("deleted"))
    }

    /// The counter filter below registers itself as a service, so a handler can read it
    /// back. Both hold the same instance.
    async fn metrics(req: Request) -> Result<Message> {
        let counter = req.context().service::<Counter>()?;
        Ok(Message::data(json!({ "hits": counter.hits() })))
    }
}

#[async_trait]
impl Rest for ApiController {
    async fn init(&self, ctx: &Context) -> Result<()> {
        // Public.
        ctx.get("/api/login", handler(Self::login))
            .with_meta(skip_auth());

        // Any authenticated caller.
        ctx.get("/api/profile", handler(Self::profile))
            .with_meta(require_auth());

        // Everything under `/api/admin` must carry `require_auth()`. Both routes below do,
        // so this changes nothing today — it is what makes a third one safe: register it
        // without `.with_meta(require_auth())` and the application refuses to start,
        // naming the route, instead of serving it unprotected.
        //
        // A guard is the requirement the whole prefix shares. The permissions are not:
        // they differ per route, and a guard that demanded one of them would refuse the
        // other.
        ctx.require_meta("/api/admin", require_auth());

        // Authenticated *and* permitted. Options chain, and are evaluated independently.
        ctx.post("/api/admin/users", handler(Self::create_user))
            .with_meta(require_auth())
            .with_meta(require_permission("admin:create_user"));

        ctx.delete("/api/admin/users/:id", handler(Self::delete_user))
            .with_meta(require_auth())
            .with_meta(require_permission("admin:delete_user"));

        // Untagged: the filter never looks at it.
        ctx.get("/api/metrics", handler(Self::metrics));

        Ok(())
    }
}

/// A filter that is also a service: every request bumps its counter, and `/api/metrics`
/// reads the same instance back out of the context.
#[derive(Default)]
struct Counter {
    hits: std::sync::atomic::AtomicU64,
}

impl Counter {
    fn hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Filter for Counter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        chain.next(req).await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);

    // Registering a filter also registers it as a service, so the metrics handler can
    // look the very same instance up again at request time.
    builder.filter(Counter::default());
    builder.filter(AuthFilter);
    builder.rest(ApiController);

    builder.build().run().await?;
    Ok(())
}
