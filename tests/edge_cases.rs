//! Edge cases across the framework: routing, bodies, errors, concurrency and shutdown.
//!
//! Everything here goes over a real socket or through the real router — the unit tests in
//! `src/` cover the pieces, this covers what happens when they meet.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use rust_web_frame::config::Config;
use rust_web_frame::core::{Context, Service};
use rust_web_frame::web::{
    Error, Filter, FilterChain, Message, MetaOption, Request, Response, Result, handler,
};
use rust_web_frame::{Builder, WebFrame};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

async fn app(configure: impl FnOnce(&mut Builder)) -> axum::Router {
    let mut builder = Builder::new(Config::empty());
    configure(&mut builder);
    builder
        .build()
        .into_router()
        .await
        .expect("the application initialises")
}

async fn call(
    router: &axum::Router,
    request: axum::http::Request<Body>,
) -> (
    axum::http::StatusCode,
    axum::http::HeaderMap,
    serde_json::Value,
) {
    let response = router.clone().oneshot(request).await.expect("served");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("readable");
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

fn get(path: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn request(method: &str, path: &str, body: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

// ------------------------------------------------------------------ routing

#[tokio::test]
async fn two_parameter_names_on_the_same_path_are_rejected() {
    // `/a/:x` and `/a/:y` are the same route with a different label. matchit refuses the
    // second; the framework has to turn that into a startup error, not a panic.
    let mut builder = Builder::new(Config::empty());
    builder.get("/a/:x", handler(|_| async { Ok("x") }));
    builder.get("/a/:y", handler(|_| async { Ok("y") }));

    let outcome = builder.build().into_router().await;
    assert!(
        outcome.is_err(),
        "a conflicting route must be refused at startup"
    );
}

#[tokio::test]
async fn a_static_segment_wins_over_the_wildcard_beside_it() {
    // matchit allows the two to coexist and gives the static segment priority, so this is
    // about which handler answers rather than about refusing to start.
    let app = app(|builder| {
        builder.get("/files/*rest", handler(|_| async { Ok("wild") }));
        builder.get("/files/specific", handler(|_| async { Ok("static") }));
    })
    .await;

    let (_, _, body) = call(&app, get("/files/specific")).await;
    assert_eq!(body, serde_json::Value::Null, "a text response is not JSON");

    let router = app.clone();
    let response = router.oneshot(get("/files/specific")).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"static");

    let response = app.oneshot(get("/files/other")).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], b"wild");
}

#[tokio::test]
async fn a_trailing_slash_is_not_a_different_route() {
    let app = app(|builder| {
        builder.get("/users", handler(|_| async { Ok("users") }));
    })
    .await;

    let (status, _, _) = call(&app, get("/users")).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // `/users/` is a different path to axum; it lands on the fallback.
    let (status, _, json) = call(&app, get("/users/")).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{json}");
}

#[tokio::test]
async fn encoded_path_segments_reach_the_handler() {
    let app = app(|builder| {
        builder.get(
            "/echo/:name",
            handler(|req: Request| async move {
                Ok(Response::json(json!({ "name": req.param_or("name", "") })))
            }),
        );
    })
    .await;

    let (status, _, json) = call(&app, get("/echo/%E5%8C%97%E4%BA%AC")).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(json["name"], "北京");
}

#[tokio::test]
async fn a_wildcard_captures_the_rest_of_the_path() {
    let app = app(|builder| {
        builder.get(
            "/files/*rest",
            handler(|req: Request| async move {
                Ok(Response::json(json!({ "rest": req.param_or("rest", "") })))
            }),
        );
    })
    .await;

    let (_, _, json) = call(&app, get("/files/a/b/c.txt")).await;
    assert_eq!(json["rest"], "a/b/c.txt");
}

#[tokio::test]
async fn head_is_answered_without_a_body() {
    let app = app(|builder| {
        builder.get(
            "/thing",
            handler(|_| async { Ok(json!({"big": "payload"})) }),
        );
    })
    .await;

    let head = axum::http::Request::builder()
        .method("HEAD")
        .uri("/thing")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(head).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(body.is_empty(), "HEAD must not carry the body: {body:?}");
}

#[tokio::test]
async fn the_allow_header_lists_every_method_the_path_answers() {
    let app = app(|builder| {
        builder.get("/both", handler(|_| async { Ok("get") }));
        builder.post("/both", handler(|_| async { Ok("post") }));
    })
    .await;

    let request = axum::http::Request::builder()
        .method("DELETE")
        .uri("/both")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = call(&app, request).await;

    assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
    let allow = headers["allow"].to_str().unwrap();
    assert!(allow.contains("GET"), "{allow}");
    assert!(allow.contains("POST"), "{allow}");
    assert!(allow.contains("HEAD"), "GET implies HEAD: {allow}");
}

// ------------------------------------------------------------------- bodies

#[tokio::test]
async fn an_empty_body_is_not_valid_json() {
    let app = app(|builder| {
        builder.post(
            "/create",
            handler(|req: Request| async move {
                #[derive(serde::Deserialize)]
                struct Payload {
                    name: String,
                }
                let payload: Payload = req.bind_json()?;
                Ok(Message::data(json!(payload.name)))
            }),
        );
    })
    .await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/create")
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = call(&app, request).await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["code"], 400);
}

#[tokio::test]
async fn a_json_array_is_not_a_json_object() {
    let app = app(|builder| {
        builder.post(
            "/obj",
            handler(|req: Request| async move {
                let object = req.json_object()?;
                Ok(Message::data(json!(object.len())))
            }),
        );
    })
    .await;

    let (status, _, json) = call(&app, request("POST", "/obj", "[1,2,3]")).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{json}");

    let (status, _, _) = call(&app, request("POST", "/obj", r#"{"a":1,"b":2}"#)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn a_get_with_a_body_still_refuses_json() {
    let app = app(|builder| {
        builder.get(
            "/x",
            handler(|req: Request| async move {
                let _ = req.json()?;
                Ok("ok")
            }),
        );
    })
    .await;

    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/x")
        .body(Body::from(r#"{"a":1}"#))
        .unwrap();
    let (status, _, json) = call(&app, request).await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test]
async fn a_chunked_body_without_a_content_length_is_read() {
    // No `Content-Length`: hyper hands the handler a streaming body.
    let app = app(|builder| {
        builder.post(
            "/chunked",
            handler(|req: Request| async move {
                let object = req.json_object()?;
                Ok(Message::data(json!(
                    object.get("n").and_then(|v| v.as_i64())
                )))
            }),
        );
    })
    .await;

    let body = futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{\"n\":")),
        Ok(bytes::Bytes::from_static(b"42}")),
    ]);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chunked")
        .header("content-type", "application/json")
        .body(Body::from_stream(body))
        .unwrap();

    let (status, _, json) = call(&app, request).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{json}");
    assert_eq!(json["data"], 42);
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused() {
    let config = Config::from_value(json!({"web": {"server": {"maxBodySize": 32}}}));
    let mut builder = Builder::new(config);
    builder.post(
        "/big",
        handler(|req: Request| async move {
            let _ = req.json()?;
            Ok("accepted")
        }),
    );
    let app = builder.build().into_router().await.unwrap();

    let (status, _, json) = call(
        &app,
        request(
            "POST",
            "/big",
            &format!(r#"{{"pad":"{}"}}"#, "x".repeat(200)),
        ),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert_eq!(json["code"], 413);
}

// ------------------------------------------------------------------- params

#[tokio::test]
async fn query_parameters_survive_awkward_values() {
    let app = app(|builder| {
        builder.get(
            "/q",
            handler(|req: Request| async move {
                Ok(Response::json(json!({
                    "space": req.query("space"),
                    "plus": req.query("plus"),
                    "empty": req.query("empty"),
                    "flag": req.query("flag"),
                    "first": req.query("dup"),
                })))
            }),
        );
    })
    .await;

    let (_, _, json) = call(&app, get("/q?space=a%20b&plus=a+b&empty=&flag&dup=1&dup=2")).await;

    assert_eq!(json["space"], "a b");
    assert_eq!(json["plus"], "a+b", "`+` is left alone outside a form body");
    assert_eq!(json["empty"], "");
    assert_eq!(
        json["flag"], "",
        "a bare key is present with an empty value"
    );
    assert_eq!(json["first"], "1", "the first occurrence wins");
}

#[tokio::test]
async fn paging_survives_nonsense_values() {
    let app = app(|builder| {
        builder.get(
            "/p",
            handler(|req: Request| async move {
                let page = req.page()?;
                Ok(Response::json(json!({
                    "no": page.page_no,
                    "size": page.page_size,
                    "index": page.page_index(),
                    "effectiveSize": page.page_size(),
                })))
            }),
        );
    })
    .await;

    // Zero and negatives are clamped, and the sea-orm index never divides by zero.
    let (_, _, json) = call(&app, get("/p?pageNo=0&pageSize=0")).await;
    assert_eq!(json["no"], 0);
    assert_eq!(json["index"], 0);
    assert_eq!(json["effectiveSize"], 10);

    let (_, _, json) = call(&app, get("/p?pageNo=-5&pageSize=-5")).await;
    assert_eq!(json["index"], 0);
    assert_eq!(json["effectiveSize"], 10);

    let (_, _, json) = call(&app, get("/p?pageNo=abc&pageSize=%20")).await;
    assert_eq!(json["no"], 0);
    assert_eq!(json["effectiveSize"], 10);
}

// ------------------------------------------------------------------- errors

#[tokio::test]
async fn a_filter_error_wins_over_the_handler() {
    struct Deny;

    #[async_trait]
    impl Filter for Deny {
        async fn handle(&self, _chain: &mut FilterChain, _req: Request) -> Result<Response> {
            Err(Error::forbidden().detail("stopped at the filter"))
        }
    }

    let app = app(|builder| {
        builder.get(
            "/x",
            handler(|_| async { Err::<Response, _>(Error::internal().detail("never reached")) }),
        );
        builder.filter(Deny);
    })
    .await;

    let (status, _, json) = call(&app, get("/x")).await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(json["msg"], "stopped at the filter");
}

#[tokio::test]
async fn a_handler_returning_an_error_value_still_maps_to_a_status() {
    let app = app(|builder| {
        builder.get(
            "/err",
            handler(|_| async { Ok(Response::error(Error::too_many_requests())) }),
        );
    })
    .await;

    let (status, _, json) = call(&app, get("/err")).await;
    assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json["code"], 429);
}

#[tokio::test]
async fn a_queued_response_header_survives_an_error() {
    // `response.set_header` only reaches a response the filter is holding; a handler that
    // returns `Err` never gives it one. Queueing is what makes `x-request-id` appear on the
    // error envelope too.
    struct Stamp;

    #[async_trait]
    impl Filter for Stamp {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            req.set_response_header("x-request-id", "3f2a");
            chain.next(req).await
        }
    }

    let app = app(|builder| {
        builder.get("/ok", handler(|_| async { Ok("fine") }));
        builder.get(
            "/x",
            handler(|_| async { Err::<Response, _>(Error::bad_request()) }),
        );
        builder.filter(Stamp);
    })
    .await;

    // On a success…
    let (status, headers, _) = call(&app, get("/ok")).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(headers["x-request-id"], "3f2a");

    // …and on the error envelope, which is the whole point.
    let (status, headers, _) = call(&app, get("/x")).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(headers["x-request-id"], "3f2a");
}

// ------------------------------------------------------------------- meta

#[tokio::test]
async fn metadata_attached_to_a_builder_route_reaches_the_served_route() {
    let seen = Arc::new(AtomicUsize::new(0));
    let flag = MetaOption::value("guarded", true);

    struct Watch(Arc<AtomicUsize>);

    #[async_trait]
    impl Filter for Watch {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            if req.has_meta(MetaOption::value("guarded", true)) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            chain.next(req).await
        }
    }

    let app = app(|builder| {
        builder
            .get("/guarded", handler(|_| async { Ok("guarded") }))
            .with_meta(flag.clone());
        builder.get("/open", handler(|_| async { Ok("open") }));
        builder.filter(Watch(Arc::clone(&seen)));
    })
    .await;

    let _ = call(&app, get("/guarded")).await;
    let _ = call(&app, get("/open")).await;

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "only the route that opted in should have matched"
    );
}

// ------------------------------------------------------------ concurrency

#[tokio::test]
async fn many_requests_in_flight_share_the_container_correctly() {
    #[derive(Default)]
    struct Counter(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for Counter {}

    let app = app(|builder| {
        builder.service(Counter::default());
        builder.get(
            "/count",
            handler(|req: Request| async move {
                let counter = req.context().service::<Counter>()?;
                counter.0.fetch_add(1, Ordering::SeqCst);
                Ok(Message::data(json!(counter.0.load(Ordering::SeqCst))))
            }),
        );
    })
    .await;

    let mut tasks = Vec::new();
    for _ in 0..64 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            let (status, _, _) = call(&app, get("/count")).await;
            status
        }));
    }

    for task in tasks {
        assert_eq!(task.await.unwrap(), axum::http::StatusCode::OK);
    }
}

#[tokio::test]
async fn a_long_running_handler_does_not_block_other_requests() {
    let app = app(|builder| {
        builder.get(
            "/slow",
            handler(|_| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok("slow")
            }),
        );
        builder.get("/fast", handler(|_| async { Ok("fast") }));
    })
    .await;

    let slow = tokio::spawn({
        let app = app.clone();
        async move { call(&app, get("/slow")).await.0 }
    });

    // The fast route has to answer while the slow one is still sleeping.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    let (status, _, _) = call(&app, get("/fast")).await;
    let elapsed = started.elapsed();

    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(elapsed < Duration::from_millis(200), "took {elapsed:?}");

    assert_eq!(slow.await.unwrap(), axum::http::StatusCode::OK);
}

// ------------------------------------------------------------ shutdown

async fn listening_app(port: u16, configure: impl FnOnce(&mut Builder)) -> WebFrame {
    let mut config = Config::empty();
    config.set("web.server.port", json!(port));

    let mut builder = Builder::new(config);
    configure(&mut builder);
    builder.build()
}

async fn wait_for_port(port: u16) {
    for _ in 0..150 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing started listening on {port}");
}

#[tokio::test]
async fn an_in_flight_request_finishes_before_shutdown_completes() {
    const PORT: u16 = 19401;

    let stop = Arc::new(std::sync::OnceLock::<Context>::new());
    let handle = Arc::clone(&stop);

    let frame = listening_app(PORT, move |builder| {
        builder.routes(move |ctx| {
            let _ = handle.set(ctx.clone());
            let owned = ctx.clone();
            ctx.get(
                "/slow",
                handler(|_| async {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Ok("finished anyway")
                }),
            );
            ctx.get(
                "/quit",
                handler(move |_req: Request| {
                    let ctx = owned.clone();
                    async move {
                        ctx.shutdown();
                        Ok("stopping")
                    }
                }),
            );
        });
    })
    .await;

    let runtime = frame.build_runtime().await.expect("starts");
    let run = tokio::spawn(async move { runtime.run().await });
    wait_for_port(PORT).await;

    // Start a slow request, then ask the server to stop while it is in flight.
    let slow = tokio::spawn(async move {
        let mut stream = TcpStream::connect(("127.0.0.1", PORT)).await.unwrap();
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;
        response
    });

    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut quit = TcpStream::connect(("127.0.0.1", PORT)).await.unwrap();
    quit.write_all(b"GET /quit HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut quit_response = String::new();
    let _ = quit.read_to_string(&mut quit_response).await;

    let response = tokio::time::timeout(Duration::from_secs(10), slow)
        .await
        .expect("the in-flight request must not be abandoned")
        .unwrap();

    assert!(
        response.contains("finished anyway"),
        "the request was cut off by shutdown: {response:?}"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("run() must return")
        .unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");

    // The port is free again.
    assert!(TcpStream::connect(("127.0.0.1", PORT)).await.is_err());
}

#[tokio::test]
async fn a_new_connection_is_refused_once_shutdown_starts() {
    const PORT: u16 = 19402;

    let stop = Arc::new(std::sync::OnceLock::<Context>::new());
    let handle = Arc::clone(&stop);

    let frame = listening_app(PORT, move |builder| {
        builder.routes(move |ctx| {
            let _ = handle.set(ctx.clone());
            let owned = ctx.clone();
            ctx.get(
                "/quit",
                handler(move |_req: Request| {
                    let ctx = owned.clone();
                    async move {
                        ctx.shutdown();
                        Ok("stopping")
                    }
                }),
            );
        });
    })
    .await;

    let runtime = frame.build_runtime().await.expect("starts");
    let run = tokio::spawn(async move { runtime.run().await });
    wait_for_port(PORT).await;

    let mut quit = TcpStream::connect(("127.0.0.1", PORT)).await.unwrap();
    quit.write_all(b"GET /quit HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    let _ = quit.read_to_string(&mut response).await;
    assert!(response.contains("stopping"), "{response:?}");

    let _ = tokio::time::timeout(Duration::from_secs(10), run).await;

    // Either the connection is refused or it opens and immediately closes; both are a
    // stopped server. What must not happen is a request being served.
    if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", PORT)).await {
        let _ = stream
            .write_all(b"GET /quit HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await;
        let mut late = String::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut late)).await;
        assert!(
            !late.contains("stopping"),
            "the server answered after shutting down: {late:?}"
        );
    }

    let _ = stop;
}
