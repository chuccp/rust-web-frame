//! Server-level behaviour that only shows up over a real socket: timeouts, the header
//! cap, CORS, and how `contextPath` and `locations` map URLs onto a route table.

use std::time::Duration;

use rust_web_frame::Builder;
use rust_web_frame::config::Config;
use rust_web_frame::web::{Message, Request, handler};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------- harness

async fn wait_for_port(port: u16) {
    for _ in 0..150 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing started listening on {port}");
}

/// Sends a raw request and returns `(status, headers, body)`.
async fn raw(port: u16, request: &str) -> (String, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.lines().next().unwrap_or_default().to_owned();
    (status, head.to_owned(), body.to_owned())
}

// ------------------------------------------------------------- timeouts

#[tokio::test]
async fn a_client_that_dribbles_its_headers_is_disconnected() {
    const PORT: u16 = 19201;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    // One second, so the test does not sit through the 15 s default.
    config.set("web.server.readHeaderTimeout", json!(1));

    let mut builder = Builder::new(config);
    builder.get("/", handler(|_| async { Ok("ok") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    // Open a connection and send a partial request head, then stall.
    let mut stream = TcpStream::connect(("127.0.0.1", PORT))
        .await
        .expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Slow: ")
        .await
        .expect("write");

    let mut response = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_to_string(&mut response),
    )
    .await;

    assert!(
        read.is_ok(),
        "the server never closed the stalled connection"
    );
    assert!(
        response.is_empty() || response.starts_with("HTTP/1.1 408"),
        "expected a disconnect or a 408, got: {response:?}"
    );
}

#[tokio::test]
async fn headers_larger_than_the_cap_are_rejected() {
    const PORT: u16 = 19202;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set("web.server.maxHeaderBytes", json!(1024));

    let mut builder = Builder::new(config);
    builder.get("/", handler(|_| async { Ok("ok") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    let (status, _, _) = raw(
        PORT,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: {}\r\nConnection: close\r\n\r\n",
            "x".repeat(8192)
        ),
    )
    .await;

    assert!(
        status.contains("431") || status.contains("400"),
        "expected the oversized head to be refused, got: {status}"
    );

    // A normal request still gets through, so the cap is not a blanket refusal.
    let (status, _, body) = raw(
        PORT,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn a_body_that_never_finishes_is_timed_out() {
    const PORT: u16 = 19203;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set("web.server.readTimeout", json!(1));

    let mut builder = Builder::new(config);
    builder.post(
        "/echo",
        handler(|req: Request| async move {
            let value: serde_json::Value = req.bind_json()?;
            Ok(Message::data(value))
        }),
    );
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    // Promise a body and then never send it.
    let mut stream = TcpStream::connect(("127.0.0.1", PORT))
        .await
        .expect("connect");
    stream
        .write_all(
            b"POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
              Content-Length: 100\r\n\r\n{\"a\":",
        )
        .await
        .expect("write");

    let mut response = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_to_string(&mut response),
    )
    .await;
    assert!(
        read.is_ok(),
        "the server waited forever for a body that never came"
    );
    assert!(
        response.contains("408") || response.is_empty(),
        "expected a 408 or a disconnect, got: {response:?}"
    );
}

#[tokio::test]
async fn a_long_lived_response_is_not_cut_off_by_the_read_timeout() {
    const PORT: u16 = 19204;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    // Deliberately shorter than the handler's own delay.
    config.set("web.server.readTimeout", json!(1));

    let mut builder = Builder::new(config);
    builder.get(
        "/slow",
        handler(|_| async {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            Ok("slow but fine")
        }),
    );
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    let (status, _, body) = raw(
        PORT,
        "GET /slow HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(
        status.contains("200"),
        "the read timeout must not bound the response: {status}"
    );
    assert_eq!(body, "slow but fine");
}

// ----------------------------------------------------------------- cors

#[tokio::test]
async fn a_preflight_is_answered_even_though_no_route_matches_it() {
    const PORT: u16 = 19205;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set(
        "web.server.cors",
        json!({
            "allowedOrigins": ["https://app.example.com"],
            "allowedMethods": ["GET", "POST"],
            "allowedHeaders": ["content-type"],
            "allowCredentials": true,
            "maxAge": 600
        }),
    );

    let mut builder = Builder::new(config);
    builder.get("/thing", handler(|_| async { Ok("thing") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    // A preflight matches no route — without the CORS layer it would be a 405.
    let (status, head, _) = raw(
        PORT,
        "OPTIONS /thing HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Origin: https://app.example.com\r\n\
         Access-Control-Request-Method: POST\r\n\
         Access-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(
        status.contains("200") || status.contains("204"),
        "preflight failed: {status}"
    );
    assert!(
        head.contains("access-control-allow-origin: https://app.example.com")
            || head.contains("Access-Control-Allow-Origin: https://app.example.com"),
        "missing allow-origin:\n{head}"
    );

    // A disallowed origin is not echoed back.
    let (_, head, _) = raw(
        PORT,
        "OPTIONS /thing HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Origin: https://evil.example.com\r\n\
         Access-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        !head.contains("evil.example.com"),
        "an unlisted origin was allowed:\n{head}"
    );
}

#[tokio::test]
async fn an_actual_request_carries_the_cors_headers() {
    const PORT: u16 = 19206;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set(
        "web.server.cors",
        json!({"allowedOrigins": ["https://app.example.com"]}),
    );

    let mut builder = Builder::new(config);
    builder.get("/thing", handler(|_| async { Ok("thing") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    let (status, head, body) = raw(
        PORT,
        "GET /thing HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Origin: https://app.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(status.contains("200"), "{status}");
    assert_eq!(body, "thing");
    assert!(head.contains("access-control-allow-origin"), "{head}");
}

#[tokio::test]
async fn no_cors_config_sends_no_cors_headers() {
    const PORT: u16 = 19207;
    let mut builder = Builder::new({
        let mut config = Config::empty();
        config.set("web.server.port", json!(PORT));
        config
    });
    builder.get("/thing", handler(|_| async { Ok("thing") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    let (_, head, _) = raw(
        PORT,
        "GET /thing HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Origin: https://app.example.com\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(
        !head.to_ascii_lowercase().contains("access-control"),
        "CORS headers appeared without any configuration:\n{head}"
    );
}

// ---------------------------------------------------------- context path

#[tokio::test]
async fn a_context_path_prefixes_routes_and_leaves_the_root_free() {
    const PORT: u16 = 19208;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set("web.server.contextPath", json!("/api"));

    let mut builder = Builder::new(config);
    builder.get("/users", handler(|_| async { Ok("users") }));
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    let (status, _, body) = raw(
        PORT,
        "GET /api/users HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, "users");

    // Outside the prefix there is no route — the root stays available for an SPA.
    let (status, _, _) = raw(
        PORT,
        "GET /users HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("404"), "{status}");
}

#[tokio::test]
async fn a_context_path_is_normalised_however_it_is_written() {
    // `api`, `/api/` and `/api` all have to produce the same route table.
    for (index, written) in ["api", "/api/", "  /api  "].iter().enumerate() {
        let port = 19210 + index as u16;

        let mut config = Config::empty();
        config.set("web.server.port", json!(port));
        config.set("web.server.contextPath", json!(written));

        let mut builder = Builder::new(config);
        builder.get("/users", handler(|_| async { Ok("users") }));
        tokio::spawn(async move {
            let _ = builder.build().build_runtime().await.unwrap().run().await;
        });
        wait_for_port(port).await;

        let (status, _, body) = raw(
            port,
            "GET /api/users HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(status.contains("200"), "written as {written:?}: {status}");
        assert_eq!(body, "users", "written as {written:?}");
    }
}

// -------------------------------------------------------------- locations

#[tokio::test]
async fn locations_map_onto_the_url_root() {
    const PORT: u16 = 19220;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("report.csv"), "a,b\n1,2\n").unwrap();

    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set(
        "web.server.locations",
        json!([dir.path().to_string_lossy()]),
    );

    let builder = Builder::new(config);
    tokio::spawn(async move {
        let _ = builder.build().build_runtime().await.unwrap().run().await;
    });
    wait_for_port(PORT).await;

    // The directory *is* the root: `./public/report.csv` is served at `/report.csv`,
    // not at `/public/report.csv`.
    let (status, _, body) = raw(
        PORT,
        "GET /report.csv HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, "a,b\n1,2\n");

    let (status, _, _) = raw(
        PORT,
        "GET /public/report.csv HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(status.contains("404"), "{status}");
}
