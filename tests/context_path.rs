//! `contextPath` across every way a route can be registered.
//!
//! The prefix is applied in one place — `Server::router` — so it should reach everything.
//! This is the test that says so, for each registration path in turn.

use std::time::Duration;

use serde_json::json;
use tower::ServiceExt;

use rust_web_frame::Builder;
use rust_web_frame::config::Config;
use rust_web_frame::core::RestGroup;
use rust_web_frame::web::{
    Message, Request, Response, Result, ServerConfig, SseStream, WebSocketStream, handler,
};

const PREFIX: &str = "/api";

async fn app(configure: impl FnOnce(&mut Builder)) -> axum::Router {
    let mut config = Config::from_value(json!({
        "web": {"server": {"contextPath": PREFIX}}
    }));
    // The test asserts on the normalised value, so it is set the way a config file would.
    let _ = &mut config;

    let mut builder = Builder::new(config);
    configure(&mut builder);
    builder.build().into_router().await.expect("initialises")
}

async fn status_of(router: &axum::Router, path: &str) -> axum::http::StatusCode {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("served");
    response.status()
}

async fn body_of(router: &axum::Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("served");
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("readable");
    String::from_utf8_lossy(&bytes).into_owned()
}

// ------------------------------------------------------------ controllers

#[tokio::test]
async fn a_controllers_routes_are_prefixed() {
    #[derive(Default)]
    struct Api;

    #[async_trait::async_trait]
    impl rust_web_frame::core::Rest for Api {
        async fn init(&self, ctx: &rust_web_frame::core::Context) -> Result<()> {
            ctx.get("/users", handler(|_| async { Ok("users") }));
            Ok(())
        }
    }

    let app = app(|builder| {
        builder.rest(Api);
    })
    .await;

    assert_eq!(
        status_of(&app, "/api/users").await,
        axum::http::StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "/users").await,
        axum::http::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_builder_route_is_prefixed() {
    let app = app(|builder| {
        builder.get("/direct", handler(|_| async { Ok("direct") }));
    })
    .await;

    assert_eq!(
        status_of(&app, "/api/direct").await,
        axum::http::StatusCode::OK
    );
    assert_eq!(body_of(&app, "/api/direct").await, "direct");
}

#[tokio::test]
async fn an_inline_routes_closure_is_prefixed() {
    let app = app(|builder| {
        builder.routes(|ctx| {
            ctx.get("/inline", handler(|_| async { Ok("inline") }));
        });
    })
    .await;

    assert_eq!(
        status_of(&app, "/api/inline").await,
        axum::http::StatusCode::OK
    );
}

#[tokio::test]
async fn a_wildcard_route_is_prefixed() {
    let app = app(|builder| {
        builder.get(
            "/files/*rest",
            handler(|req: Request| async move {
                Ok(Response::json(json!({ "rest": req.param_or("rest", "") })))
            }),
        );
    })
    .await;

    let body = body_of(&app, "/api/files/a/b.txt").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["rest"], "a/b.txt");

    assert_eq!(
        status_of(&app, "/files/a/b.txt").await,
        axum::http::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_redirect_is_prefixed() {
    let app = app(|builder| {
        builder.routes(|ctx| {
            ctx.redirect("/old", "/api/new");
            ctx.get("/new", handler(|_| async { Ok("new") }));
        });
    })
    .await;

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/old")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 301);

    assert_eq!(
        status_of(&app, "/old").await,
        axum::http::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_health_check_is_prefixed() {
    let app = app(|builder| {
        builder.routes(|ctx| {
            ctx.health_check();
        });
    })
    .await;

    assert_eq!(
        status_of(&app, "/api/healthz").await,
        axum::http::StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "/healthz").await,
        axum::http::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn mounted_static_files_are_prefixed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.js"), "console.log(1)").unwrap();
    let root = dir.path().to_path_buf();

    let app = app(move |builder| {
        builder.routes(move |ctx| {
            ctx.static_files("/assets", root.clone());
        });
    })
    .await;

    assert_eq!(
        status_of(&app, "/api/assets/app.js").await,
        axum::http::StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "/assets/app.js").await,
        axum::http::StatusCode::NOT_FOUND
    );
}

// --------------------------------------------------------------- fallback

#[tokio::test]
async fn a_static_location_is_served_at_the_root_and_under_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>spa</h1>").unwrap();

    let mut config = Config::from_value(json!({
        "web": {"server": {
            "contextPath": PREFIX,
            "locations": [dir.path().to_string_lossy()],
            "page404": dir.path().join("index.html").to_string_lossy()
        }}
    }));
    let _ = &mut config;

    let builder = Builder::new(config);
    let app = builder.build().into_router().await.unwrap();

    // The API lives under the prefix; the SPA lives at the root. Both have to work, which
    // is the reason the fallback strips the prefix only when it is actually there.
    assert_eq!(body_of(&app, "/index.html").await, "<h1>spa</h1>");
    assert_eq!(body_of(&app, "/api/index.html").await, "<h1>spa</h1>");
}

#[tokio::test]
async fn the_spa_fallback_answers_a_root_navigation_without_touching_the_api() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>spa</h1>").unwrap();

    let config = Config::from_value(json!({
        "web": {"server": {
            "contextPath": PREFIX,
            "locations": [dir.path().to_string_lossy()],
            "page404": dir.path().join("index.html").to_string_lossy()
        }}
    }));

    let mut builder = Builder::new(config);
    builder.get("/data", handler(|_| async { Ok(json!({"api": true})) }));
    let app = builder.build().into_router().await.unwrap();

    // A browser navigation to an unknown deep link gets the SPA…
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/dashboard/settings")
                .header("accept", "text/html")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"<h1>spa</h1>");

    // …and the API still answers under its own prefix.
    assert_eq!(
        status_of(&app, "/api/data").await,
        axum::http::StatusCode::OK
    );

    // An unmatched path under the prefix is still a 404 for a fetch, not the SPA.
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/nothing-here")
                .header("accept", "application/json")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------ rest groups

#[tokio::test]
async fn a_rest_group_keeps_its_own_prefix_alongside_the_default_servers() {
    const PORT: u16 = 19501;
    const GROUP_PORT: u16 = 19502;

    let config = Config::from_value(json!({
        "web": {"server": {"port": PORT, "contextPath": "/api"}}
    }));

    let mut builder = Builder::new(config);
    builder.get("/here", handler(|_| async { Ok("default ok") }));
    builder.rest_group(
        RestGroup::builder("admin")
            .server_config(ServerConfig::on_port(GROUP_PORT).with_context_path("/admin"))
            .routes(|ctx| {
                ctx.get("/ping", handler(|_| async { Ok("admin ok") }));
            })
            .build(),
    );

    let runtime = builder.build().build_runtime().await.expect("starts");
    tokio::spawn(async move {
        let _ = runtime.run().await;
    });

    for port in [PORT, GROUP_PORT] {
        for _ in 0..150 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    assert_eq!(fetch(PORT, "/api/here").await, "default ok");
    assert_eq!(fetch(GROUP_PORT, "/admin/ping").await, "admin ok");

    // Neither prefix leaks onto the other server.
    assert!(fetch(PORT, "/admin/ping").await.contains("404"));
    assert!(fetch(GROUP_PORT, "/api/here").await.contains("404"));
    assert!(fetch(GROUP_PORT, "/ping").await.contains("404"));
}

async fn fetch(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or(response)
}

// ------------------------------------------------------------- streaming

#[tokio::test]
async fn sse_and_websocket_routes_are_prefixed() {
    let app = app(|builder| {
        builder.routes(|ctx| {
            ctx.sse("/events", |stream: SseStream| {
                Box::pin(async move {
                    stream.send("tick", "1");
                    Ok(())
                })
            });
            ctx.websocket("/ws", |mut socket: WebSocketStream| {
                Box::pin(async move {
                    while let Some(frame) = socket.read().await {
                        if frame.is_close() {
                            break;
                        }
                    }
                    Ok(())
                })
            });
        });
    })
    .await;

    // The SSE endpoint exists under the prefix…
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/event-stream",
        "an SSE route must answer with an event stream"
    );

    // …and not without it.
    assert_eq!(
        status_of(&app, "/events").await,
        axum::http::StatusCode::NOT_FOUND
    );

    // The WebSocket route is registered under the prefix too. A plain GET is not an
    // upgrade, so the framework answers with its own 400 rather than a 404 — which is how
    // this asserts the route was found at all.
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/ws")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_page_size_request_survives_the_prefix() {
    let app = app(|builder| {
        builder.get(
            "/paged",
            handler(|req: Request| async move {
                let page = req.page()?;
                Ok(Message::data(json!({
                    "path": req.path(),
                    "contextPath": req.context_path(),
                    "size": page.page_size(),
                })))
            }),
        );
    })
    .await;

    let body = body_of(&app, "/api/paged?pageSize=25").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();

    // `path()` is the whole path; `context_path()` reports the prefix in its normalised
    // form, which is what a handler would use to build an absolute link.
    assert_eq!(json["data"]["path"], "/api/paged");
    assert_eq!(json["data"]["contextPath"], "/api");
    assert_eq!(json["data"]["size"], 25);
}
