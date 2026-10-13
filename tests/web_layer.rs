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
            response.set_header("x-served-by", "rust-web-frame");
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

// ------------------------------------------------------------------- uploads

/// Builds a `multipart/form-data` body plus its content type.
fn multipart(parts: &[(&str, Option<&str>, &str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "----RustWebFrameTestBoundary";
    let mut body = Vec::new();
    for (name, file_name, content_type, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match file_name {
            Some(file_name) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        if !content_type.is_empty() {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn upload_request(path: &str, content_type: &str, body: Vec<u8>) -> HttpRequest<Body> {
    HttpRequest::builder()
        .method("POST")
        .uri(path)
        .header("content-type", content_type)
        .body(Body::from(body))
        .expect("valid request")
}

#[tokio::test]
async fn an_uploaded_file_is_streamed_parsed_and_saved() {
    let uploads = tempfile::tempdir().unwrap();
    let directory = uploads.path().to_path_buf();

    let app = app(|builder| {
        let directory = directory.clone();
        builder.routes(move |ctx| {
            let directory = directory.clone();
            ctx.post(
                "/upload",
                handler(move |req: Request| {
                    let directory = directory.clone();
                    async move {
                        let form = req.multipart().await?;
                        let caption = form.text_or("caption", "").to_owned();
                        let file = form
                            .file("avatar")
                            .ok_or_else(|| Error::bad_request().detail("avatar is required"))?;

                        let saved = file.copy_to_dir(&directory).await?;
                        Ok(Response::json(json!({
                            "caption": caption,
                            "fileName": file.sanitised_file_name(),
                            "contentType": file.content_type(),
                            "size": file.size(),
                            "saved": saved.display().to_string(),
                        })))
                    }
                }),
            );
        });
    })
    .await;

    let (content_type, body) = multipart(&[
        ("caption", None, "", b"a nice photo"),
        ("avatar", Some("me.png"), "image/png", b"PNGDATA"),
    ]);
    let (status, json) = call(&app, upload_request("/upload", &content_type, body)).await;

    assert_eq!(status, StatusCode::OK, "got {json}");
    assert_eq!(json["caption"], "a nice photo");
    assert_eq!(json["fileName"], "me.png");
    assert_eq!(json["contentType"], "image/png");
    assert_eq!(json["size"], 7);
    assert_eq!(
        tokio::fs::read(directory.join("me.png")).await.unwrap(),
        b"PNGDATA"
    );
}

#[tokio::test]
async fn a_large_upload_spills_to_a_temporary_file_and_reads_back() {
    let app = app(|builder| {
        // A 1 KiB in-memory budget, so anything bigger has to go to disk.
        builder.routes(|ctx| {
            ctx.post(
                "/size",
                handler(|req: Request| async move {
                    let form = req.multipart().await?;
                    let file = form
                        .file("blob")
                        .ok_or_else(|| Error::bad_request().detail("blob is required"))?;
                    Ok(Response::json(json!({
                        "size": file.size(),
                        "readBack": file.bytes().await?.len(),
                    })))
                }),
            );
        });
    })
    .await;

    let payload = vec![b'x'; 8192];
    let (content_type, body) = multipart(&[(
        "blob",
        Some("big.bin"),
        "application/octet-stream",
        &payload,
    )]);
    let (status, json) = call(&app, upload_request("/size", &content_type, body)).await;

    assert_eq!(status, StatusCode::OK, "got {json}");
    assert_eq!(json["size"], 8192);
    assert_eq!(json["readBack"], 8192);
}

#[tokio::test]
async fn a_hostile_file_name_cannot_escape_the_upload_directory() {
    let root = tempfile::tempdir().unwrap();
    // Uploads land in a subdirectory, so a successful traversal would be visible as a
    // file appearing next to it.
    let directory = root.path().join("uploads");
    let sentinel = root.path().join("escaped.txt");

    let app = app(|builder| {
        let directory = directory.clone();
        builder.routes(move |ctx| {
            let directory = directory.clone();
            ctx.post(
                "/upload",
                handler(move |req: Request| {
                    let directory = directory.clone();
                    async move {
                        let form = req.multipart().await?;
                        let file = form
                            .file("doc")
                            .ok_or_else(|| Error::bad_request().detail("doc is required"))?;
                        let saved = file.copy_to_dir(&directory).await?;
                        Ok(Response::json(
                            json!({ "saved": saved.display().to_string() }),
                        ))
                    }
                }),
            );
        });
    })
    .await;

    let (content_type, body) =
        multipart(&[("doc", Some("../../escaped.txt"), "text/plain", b"pwned")]);
    let (status, json) = call(&app, upload_request("/upload", &content_type, body)).await;

    assert_eq!(status, StatusCode::OK);
    let saved = std::path::PathBuf::from(json["saved"].as_str().unwrap());
    assert!(saved.starts_with(&directory), "escaped to {saved:?}");
    assert_eq!(saved.file_name().unwrap(), "escaped.txt");
    assert!(!sentinel.exists(), "the traversal must not have happened");
    assert_eq!(tokio::fs::read(&saved).await.unwrap(), b"pwned");
}

#[tokio::test]
async fn an_oversized_upload_is_rejected_with_413() {
    let config = Config::from_value(json!({
        "web": { "server": { "maxBodySize": 256 } }
    }));
    let mut builder = Builder::new(config);
    builder.routes(|ctx| {
        ctx.post(
            "/upload",
            handler(|req: Request| async move {
                let form = req.multipart().await?;
                Ok(Response::json(json!({ "files": form.file_count() })))
            }),
        );
    });
    let app = builder.build().into_router().await.unwrap();

    let payload = vec![b'x'; 4096];
    let (content_type, body) = multipart(&[("blob", Some("big.bin"), "", &payload)]);
    let (status, json) = call(&app, upload_request("/upload", &content_type, body)).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "got {json}");
    assert_eq!(json["code"], 413);
    assert!(json["msg"].as_str().unwrap().contains("maxBodySize"));
}

#[tokio::test]
async fn a_request_body_cannot_be_read_twice() {
    let app = app(|builder| {
        builder.routes(|ctx| {
            ctx.post(
                "/twice",
                handler(|req: Request| async move {
                    let _ = req.multipart().await?;
                    // The stream is gone; reading it again has to say so.
                    req.read_body().await?;
                    Ok(Response::json(json!({ "ok": true })))
                }),
            );
        });
    })
    .await;

    let (content_type, body) = multipart(&[("a", None, "", b"hello")]);
    let (status, json) = call(&app, upload_request("/twice", &content_type, body)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json["msg"]
            .as_str()
            .unwrap()
            .contains("already been consumed")
    );
}

#[tokio::test]
async fn ordinary_bodies_still_buffer_exactly_as_before() {
    let app = app(|builder| {
        builder.post(
            "/json",
            handler(|req: Request| async move {
                let payload: CreateUser = req.bind_json()?;
                Ok(Message::data(json!({ "name": payload.name })))
            }),
        );
        builder.get(
            "/raw",
            handler(|req: Request| async move {
                Ok(Response::json(json!({
                    "buffered": req.body().is_some(),
                    "length": req.body().map(|b| b.len()).unwrap_or(0),
                })))
            }),
        );
    })
    .await;

    let (status, json) = call(
        &app,
        post_json("/json", json!({"name": "alice", "age": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"]["name"], "alice");

    let (_, json) = call(&app, get("/raw")).await;
    assert_eq!(json["buffered"], true);
    assert_eq!(json["length"], 0);
}

// ------------------------------------------------------------------- panics

#[tokio::test]
async fn a_panicking_handler_answers_with_the_envelope() {
    async fn boom(_req: Request) -> Result<Response> {
        panic!("the handler exploded");
    }

    let app = app(|builder| {
        builder.get("/boom", handler(boom));
    })
    .await;

    let (status, json) = call(&app, get("/boom")).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json["code"], 500,
        "a panic must still produce the standard envelope"
    );
    // The panic message belongs in the log, not in the response.
    assert!(
        !json["msg"].as_str().unwrap().contains("exploded"),
        "the panic message leaked to the client: {json}"
    );
}

#[tokio::test]
async fn a_panic_does_not_take_the_server_with_it() {
    async fn boom(_req: Request) -> Result<Response> {
        panic!("the handler exploded");
    }

    let app = app(|builder| {
        builder.get("/boom", handler(boom));
        builder.get("/ok", handler(|_| async { Ok("still here") }));
    })
    .await;

    let (status, _) = call(&app, get("/boom")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let (status, _) = call(&app, get("/ok")).await;
    assert_eq!(status, StatusCode::OK);
}

// -------------------------------------------------------------- static files

/// Serves `./public` under `/assets`, with a directory index inside it.
async fn static_app(root: &std::path::Path) -> axum::Router {
    let root = root.to_path_buf();
    app(move |builder| {
        let root = root.clone();
        builder.routes(move |ctx| {
            ctx.static_files("/assets", root.clone());
        });
    })
    .await
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello static").unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![b'x'; 4096]).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/index.html"), "<h1>index</h1>").unwrap();
    dir
}

#[tokio::test]
async fn a_static_file_is_served_with_its_content_type() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let response = app.clone().oneshot(get("/assets/hello.txt")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert!(
        response.headers().contains_key("etag"),
        "an ETag is what makes conditional requests possible"
    );
    assert!(response.headers().contains_key("last-modified"));
    assert_eq!(response.headers()["accept-ranges"], "bytes");

    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"hello static");
}

#[tokio::test]
async fn a_static_file_answers_conditional_requests() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let first = app.clone().oneshot(get("/assets/hello.txt")).await.unwrap();
    let etag = first.headers()["etag"].clone().to_str().unwrap().to_owned();

    let request = HttpRequest::builder()
        .uri("/assets/hello.txt")
        .header("if-none-match", &etag)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

    let modified = first.headers()["last-modified"].clone();
    let request = HttpRequest::builder()
        .uri("/assets/hello.txt")
        .header("if-modified-since", modified)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_static_file_answers_range_requests() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let request = HttpRequest::builder()
        .uri("/assets/big.bin")
        .header("range", "bytes=0-9")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 0-9/4096");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(body.len(), 10);
}

#[tokio::test]
async fn a_head_request_for_a_static_file_has_no_body() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let request = HttpRequest::builder()
        .method("HEAD")
        .uri("/assets/big.bin")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "4096");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(body.is_empty(), "HEAD must not carry the file");
}

#[tokio::test]
async fn a_directory_serves_its_index() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let response = app.oneshot(get("/assets/sub/index.html")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"<h1>index</h1>");
}

#[tokio::test]
async fn a_missing_static_file_is_the_envelope_404() {
    let dir = fixture();
    let app = static_app(dir.path()).await;

    let (status, json) = call(&app, get("/assets/nope.txt")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["code"], 404);
}

#[tokio::test]
async fn a_static_path_cannot_escape_its_root() {
    let dir = fixture();
    let secret = dir.path().parent().unwrap().join("outside.txt");
    std::fs::write(&secret, "secret").unwrap();

    let app = static_app(dir.path()).await;
    let (status, _) = call(&app, get("/assets/../outside.txt")).await;

    assert!(
        status.is_client_error(),
        "expected a rejection, got {status}"
    );
    let _ = std::fs::remove_file(secret);
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
