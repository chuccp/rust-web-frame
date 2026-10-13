//! End-to-end tests of the HTTP layer, driven through the real axum router.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use rust_web_frame::Builder;
use rust_web_frame::config::Config;
use rust_web_frame::core::{Context, Rest, Service};
use rust_web_frame::web::{
    Error, Filter, FilterChain, Message, MetaOption, Request, Response, Result, handler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower::ServiceExt;

/// Builds an application, initialises it, and returns the router the runtime would
/// serve — so the tests drive exactly what production serves.
async fn app(configure: impl FnOnce(&mut Builder)) -> axum::Router {
    let mut builder = Builder::new(Config::empty());
    configure(&mut builder);
    builder
        .build()
        .into_router()
        .await
        .expect("the application initialises")
}

/// Builds an application and expects initialisation to fail.
async fn app_error(configure: impl FnOnce(&mut Builder)) -> Error {
    let mut builder = Builder::new(Config::empty());
    configure(&mut builder);
    match builder.build().into_router().await {
        Ok(_) => panic!("expected initialisation to fail"),
        Err(error) => error,
    }
}

/// Sends one request through the router and returns `(status, json)`.
async fn call(
    router: &axum::Router,
    request: HttpRequest<Body>,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("request is served");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body is readable");
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Reads a header off a one-shot response.
async fn header(router: &axum::Router, request: HttpRequest<Body>, name: &str) -> Option<String> {
    let response = router.clone().oneshot(request).await.expect("served");
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn get(path: &str) -> HttpRequest<Body> {
    HttpRequest::builder()
        .uri(path)
        .body(Body::empty())
        .expect("valid request")
}

fn post_json(path: &str, body: serde_json::Value) -> HttpRequest<Body> {
    HttpRequest::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("valid request")
}

// ---------------------------------------------------------------- hello world

#[tokio::test]
async fn a_plain_route_answers() {
    let app = app(|builder| {
        builder.get("/", handler(|_| async { Ok("hello world") }));
    })
    .await;

    let (status, _) = call(&app, get("/")).await;
    assert_eq!(status, StatusCode::OK, "text responses are 200");
}

#[tokio::test]
async fn unmatched_routes_get_the_envelope_404() {
    let app = app(|builder| {
        builder.get("/", handler(|_| async { Ok("hello") }));
    })
    .await;

    let (status, json) = call(&app, get("/nowhere")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["code"], 404);
    assert!(json["msg"].as_str().unwrap().contains("/nowhere"));
}

#[tokio::test]
async fn a_wrong_method_on_a_known_path_is_not_found() {
    let app = app(|builder| {
        builder.get("/only-get", handler(|_| async { Ok("hi") }));
    })
    .await;

    let request = HttpRequest::builder()
        .method("POST")
        .uri("/only-get")
        .body(Body::empty())
        .unwrap();
    let (status, json) = call(&app, request).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(json["code"], 405);
}

// ------------------------------------------------------------- request access

#[tokio::test]
async fn path_query_and_header_accessors_work() {
    let app = app(|builder| {
        builder.get(
            "/users/:id",
            handler(|req: Request| async move {
                Ok(Response::json(json!({
                    "id": req.param_u64("id"),
                    "name": req.query("name"),
                    "page": req.query_i64("page"),
                    "agent": req.header("x-agent"),
                    "route": req.full_path(),
                })))
            }),
        );
    })
    .await;

    let request = HttpRequest::builder()
        .uri("/users/42?name=alice&page=3")
        .header("x-agent", "test")
        .body(Body::empty())
        .unwrap();
    let (status, json) = call(&app, request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["id"], 42);
    assert_eq!(json["name"], "alice");
    assert_eq!(json["page"], 3);
    assert_eq!(json["agent"], "test");
    assert_eq!(json["route"], "/users/:id");
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct CreateUser {
    name: String,
    age: i32,
}

#[tokio::test]
async fn json_bodies_bind_into_typed_structs() {
    let app = app(|builder| {
        builder.post(
            "/users",
            handler(|req: Request| async move {
                let payload: CreateUser = req.bind_json()?;
                Ok(Message::data(
                    json!({ "name": payload.name, "age": payload.age }),
                ))
            }),
        );
    })
    .await;

    let (status, json) = call(
        &app,
        post_json("/users", json!({"name": "alice", "age": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["code"], 200);
    assert_eq!(json["data"]["name"], "alice");
    assert_eq!(json["data"]["age"], 30);
}

#[tokio::test]
async fn a_malformed_body_is_a_bad_request() {
    let app = app(|builder| {
        builder.post(
            "/users",
            handler(|req: Request| async move {
                let payload: CreateUser = req.bind_json()?;
                Ok(Response::json(json!(payload.name)))
            }),
        );
    })
    .await;

    let (status, json) = call(&app, post_json("/users", json!({"name": 5}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["code"], 400);
}

// ------------------------------------------------------------------- errors

#[tokio::test]
async fn error_codes_map_to_http_statuses() {
    async fn missing(_req: Request) -> Result<Response> {
        Err(Error::not_found().detail("user 7 does not exist"))
    }
    async fn forbidden(_req: Request) -> Result<Response> {
        Err(Error::forbidden())
    }
    async fn business(_req: Request) -> Result<Response> {
        Err(Error::token_expired())
    }
    async fn boom(_req: Request) -> Result<Response> {
        Err("database is on fire".into())
    }

    let app = app(|builder| {
        builder.get("/missing", handler(missing));
        builder.get("/forbidden", handler(forbidden));
        builder.get("/business", handler(business));
        builder.get("/boom", handler(boom));
    })
    .await;

    let (status, json) = call(&app, get("/missing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["msg"], "user 7 does not exist");

    let (status, _) = call(&app, get("/forbidden")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Business codes travel over HTTP 200 so proxies do not treat them as failures.
    let (status, json) = call(&app, get("/business")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["code"], 1003);

    let (status, json) = call(&app, get("/boom")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["msg"], "database is on fire");
}

// ------------------------------------------------------- per-route metadata

fn require_auth() -> MetaOption {
    MetaOption::value("require_auth", true)
}

fn public() -> MetaOption {
    MetaOption::key(["skip_auth"])
}

/// One filter that enforces a policy only on the routes that opted in.
#[derive(Default)]
struct AuthFilter {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Service for AuthFilter {}

#[async_trait]
impl Filter for AuthFilter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        if !req.has_meta(require_auth()) || req.has_meta(public()) {
            return chain.next(req).await; // the route did not opt in
        }

        match req.header("authorization") {
            Some("secret") => chain.next(req).await,
            Some(_) => Err(Error::forbidden().detail("bad token")),
            None => Err(Error::unauthorized()),
        }
    }
}

#[tokio::test]
async fn metadata_gates_routes_that_opted_in() {
    let filter = AuthFilter::default();
    let calls = Arc::clone(&filter.calls);

    let app = app(|builder| {
        builder
            .get("/profile", handler(|_| async { Ok("profile") }))
            .with_meta(require_auth());
        builder.get("/login", handler(|_| async { Ok("login") }));
        builder
            .get("/health", handler(|_| async { Ok("ok") }))
            .with_meta(public());
        builder.filter(filter);
    })
    .await;

    // No token: rejected.
    let (status, json) = call(&app, get("/profile")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["code"], 401);

    // Wrong token: rejected differently.
    let request = HttpRequest::builder()
        .uri("/profile")
        .header("authorization", "nope")
        .body(Body::empty())
        .unwrap();
    let (status, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Right token: through.
    let request = HttpRequest::builder()
        .uri("/profile")
        .header("authorization", "secret")
        .body(Body::empty())
        .unwrap();
    let (status, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK);

    // A route that never opted in is untouched, even without a token.
    let (status, _) = call(&app, get("/login")).await;
    assert_eq!(status, StatusCode::OK);

    // `with_meta(public())` opted out explicitly.
    let (status, _) = call(&app, get("/health")).await;
    assert_eq!(status, StatusCode::OK);

    // The filter ran for every request, not just the guarded one.
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn a_filter_can_post_process_the_response() {
    struct Stamp;

    #[async_trait]
    impl Service for Stamp {}

    #[async_trait]
    impl Filter for Stamp {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            let mut response = chain.next(req).await?;
            response.try_set_header("x-served-by", "rust-web-frame");
            Ok(response)
        }
    }

    let app = app(|builder| {
        builder.get("/", handler(|_| async { Ok("hello") }));
        builder.filter(Stamp);
    })
    .await;

    assert_eq!(
        header(&app, get("/"), "x-served-by").await.as_deref(),
        Some("rust-web-frame")
    );
}

#[tokio::test]
async fn a_filter_can_hand_data_to_the_handler() {
    struct InjectUserId;

    #[async_trait]
    impl Service for InjectUserId {}

    #[async_trait]
    impl Filter for InjectUserId {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            req.set_attr("user_id", 7u64);
            chain.next(req).await
        }
    }

    let app = app(|builder| {
        builder.get(
            "/me",
            handler(|req: Request| async move {
                let id = req.attr::<u64>("user_id").map(|v| *v).unwrap_or(0);
                Ok(Response::json(json!({ "userId": id })))
            }),
        );
        builder.filter(InjectUserId);
    })
    .await;

    let (_, json) = call(&app, get("/me")).await;
    assert_eq!(json["userId"], 7);
}

#[tokio::test]
async fn a_registered_filter_is_also_a_service() {
    /// A filter that counts requests and is read back by a handler.
    #[derive(Default)]
    struct Hits(std::sync::atomic::AtomicU64);

    #[async_trait]
    impl Filter for Hits {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            self.0.fetch_add(1, Ordering::SeqCst);
            chain.next(req).await
        }
    }

    async fn metric(req: Request) -> Result<Response> {
        // The very same instance the filter chain holds.
        let hits = req.context().service::<Hits>()?;
        Ok(Response::json(
            json!({ "hits": hits.0.load(Ordering::SeqCst) }),
        ))
    }

    let app = app(|builder| {
        builder.filter(Hits::default());
        builder.get("/metric", handler(metric));
    })
    .await;

    let _ = call(&app, get("/metric")).await;
    let _ = call(&app, get("/metric")).await;
    let (_, json) = call(&app, get("/metric")).await;
    // The filter ran for all three requests, including the one reporting the count.
    assert_eq!(json["hits"], 3);
}

#[tokio::test]
async fn a_registered_controller_is_looked_up_by_its_own_type() {
    #[derive(Default)]
    struct Probe {
        seen: std::sync::atomic::AtomicU64,
    }

    #[async_trait]
    impl Rest for Probe {
        async fn init(&self, ctx: &Context) -> Result<()> {
            let owned = ctx.clone();
            ctx.get(
                "/probe",
                handler(move |_req: Request| {
                    let probe = owned
                        .service::<Probe>()
                        .expect("registered under its own type");
                    probe.seen.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(Response::json(json!({ "ok": true }))) }
                }),
            );
            Ok(())
        }
    }

    let app = app(|builder| {
        builder.rest(Probe::default());
    })
    .await;

    let (status, _) = call(&app, get("/probe")).await;
    assert_eq!(status, StatusCode::OK);
}

// --------------------------------------------------------------- controllers

#[derive(Default)]
struct UserController;

#[async_trait]
impl Rest for UserController {
    async fn init(&self, ctx: &Context) -> Result<()> {
        ctx.get("/api/users", handler(Self::list))
            .with_meta(public());
        ctx.get("/api/users/:id", handler(Self::show));
        ctx.post("/api/users", handler(Self::create));
        Ok(())
    }
}

impl UserController {
    async fn list(_req: Request) -> Result<Message> {
        Ok(Message::data(json!({ "users": ["alice", "bob"] })))
    }

    async fn show(req: Request) -> Result<Message> {
        Ok(Message::data(json!({ "id": req.param_u64("id") })))
    }

    async fn create(req: Request) -> Result<Message> {
        let payload: CreateUser = req.bind_json()?;
        Ok(Message::data(json!({ "created": payload.name })))
    }
}

#[tokio::test]
async fn a_controller_registers_its_own_routes() {
    let app = app(|builder| {
        builder.rest(UserController);
    })
    .await;

    let (_, json) = call(&app, get("/api/users")).await;
    assert_eq!(json["data"]["users"][0], "alice");

    let (_, json) = call(&app, get("/api/users/9")).await;
    assert_eq!(json["data"]["id"], 9);

    let (status, json) = call(
        &app,
        post_json("/api/users", json!({"name": "carol", "age": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["created"], "carol");
}

// ------------------------------------------------------------ configuration

#[tokio::test]
async fn the_context_path_prefixes_every_route() {
    let config = Config::from_value(json!({
        "web": { "server": { "port": 19009, "contextPath": "/api" } }
    }));

    let mut builder = Builder::new(config);
    builder.get("/users", handler(|_| async { Ok("users") }));
    let router = builder.build().into_router().await.unwrap();

    let response = router.clone().oneshot(get("/api/users")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = router.oneshot(get("/users")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_routes_are_rejected_at_startup() {
    let error = app_error(|builder| {
        builder.get("/same", handler(|_| async { Ok("a") }));
        builder.get("/same", handler(|_| async { Ok("b") }));
    })
    .await;

    assert!(error.client_message().contains("/same"));
}

// ------------------------------------------------------------- other bodies

#[tokio::test]
async fn redirects_and_plain_text_bodies_work() {
    let app = app(|builder| {
        builder.get(
            "/old",
            handler(|_| async { Ok(Response::redirect("/new")) }),
        );
        builder.get("/text", handler(|_| async { Ok(Response::text("plain")) }));
    })
    .await;

    assert_eq!(
        header(&app, get("/old"), "location").await.as_deref(),
        Some("/new")
    );
    assert_eq!(
        header(&app, get("/text"), "content-type").await.as_deref(),
        Some("text/plain; charset=utf-8")
    );
}

#[tokio::test]
async fn cookies_set_by_a_handler_travel_back() {
    let app = app(|builder| {
        builder.post(
            "/login",
            handler(|req: Request| async move {
                req.set_cookie("session", "abc123", 3600);
                Ok(Message::ok())
            }),
        );
        builder.get(
            "/whoami",
            handler(|req: Request| async move {
                Ok(Response::json(json!({ "session": req.cookie("session") })))
            }),
        );
    })
    .await;

    let cookie = header(&app, post_json("/login", json!({})), "set-cookie")
        .await
        .expect("a cookie is set");
    assert!(cookie.contains("session=abc123"));
    assert!(cookie.contains("HttpOnly"));

    let request = HttpRequest::builder()
        .uri("/whoami")
        .header("cookie", "session=abc123")
        .body(Body::empty())
        .unwrap();
    let (_, json) = call(&app, request).await;
    assert_eq!(json["session"], "abc123");
}
