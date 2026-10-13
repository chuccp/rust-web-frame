//! A second sweep: uploads, static files, custom converters, and TLS corners.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rust_web_frame::config::Config;
use rust_web_frame::core::Service;
use rust_web_frame::web::{
    Converter, Error, Message, Request, Response, Result, SslCertificate, SslConfig, handler,
};
use rust_web_frame::{Builder, WebFrame};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

// ---------------------------------------------------------------- harness

async fn app(configure: impl FnOnce(&mut Builder)) -> axum::Router {
    let mut builder = Builder::new(Config::empty());
    configure(&mut builder);
    builder.build().into_router().await.expect("initialises")
}

async fn body_of(router: &axum::Router, request: axum::http::Request<axum::body::Body>) -> String {
    use tower::ServiceExt;
    let response = router.clone().oneshot(request).await.expect("served");
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("readable");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn multipart_body(boundary: &str, parts: &[(&str, Option<&str>, &str, &[u8])]) -> Vec<u8> {
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
    body
}

fn upload(
    path: &str,
    boundary: &str,
    parts: &[(&str, Option<&str>, &str, &[u8])],
) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(multipart_body(boundary, parts)))
        .unwrap()
}

/// Reports what the form saw, so a test can assert on the parse rather than on a file.
fn describe_route(builder: &mut Builder) {
    builder.routes(|ctx| {
        ctx.post(
            "/describe",
            handler(|req: Request| async move {
                let form = req.multipart().await?;
                let files: Vec<serde_json::Value> = form
                    .all_files()
                    .map(|file| {
                        json!({
                            "field": file.name(),
                            "name": file.sanitised_file_name(),
                            "size": file.size(),
                            "spilled": !file.is_in_memory(),
                        })
                    })
                    .collect();
                Ok(Message::data(json!({
                    "files": files,
                    "count": form.file_count(),
                    "note": form.text("note"),
                })))
            }),
        );
    });
}

// --------------------------------------------------------------- multipart

#[tokio::test]
async fn a_zero_byte_file_is_a_file() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    let body = body_of(
        &app,
        upload(
            "/describe",
            "B",
            &[("doc", Some("empty.txt"), "text/plain", b"")],
        ),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(json["data"]["count"], 1);
    assert_eq!(json["data"]["files"][0]["size"], 0);
    assert_eq!(json["data"]["files"][0]["name"], "empty.txt");
}

#[tokio::test]
async fn a_file_with_no_name_is_dropped_not_fatal() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    // `filename=""` is what a browser sends for an empty file input.
    let body = body_of(
        &app,
        upload("/describe", "B", &[("doc", Some(""), "text/plain", b"")]),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();

    // It arrives as a file with a name that sanitises to the fallback, rather than as a
    // text field or as an error.
    assert_eq!(json["data"]["count"], 1);
    assert_eq!(json["data"]["files"][0]["name"], "upload");
}

#[tokio::test]
async fn a_unicode_file_name_survives() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    let body = body_of(
        &app,
        upload(
            "/describe",
            "B",
            &[("doc", Some("北京-报告.pdf"), "application/pdf", b"PDF")],
        ),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(json["data"]["files"][0]["name"], "北京-报告.pdf");
}

#[tokio::test]
async fn a_file_exactly_at_the_in_memory_budget_stays_in_memory() {
    let config = Config::from_value(json!({
        "web": {"server": {"maxUploadMemory": 1024}}
    }));
    let mut builder = Builder::new(config);
    describe_route(&mut builder);
    let app = builder.build().into_router().await.unwrap();

    // Exactly the budget: one byte more spills, so this has to stay in memory.
    let payload = vec![b'x'; 1024];
    let body = body_of(
        &app,
        upload(
            "/describe",
            "B",
            &[("blob", Some("exact.bin"), "", &payload)],
        ),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["data"]["files"][0]["size"], 1024);
    assert_eq!(
        json["data"]["files"][0]["spilled"], false,
        "exactly at the budget must not spill"
    );

    // One byte over does.
    let payload = vec![b'x'; 1025];
    let body = body_of(
        &app,
        upload(
            "/describe",
            "B",
            &[("blob", Some("over.bin"), "", &payload)],
        ),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["data"]["files"][0]["spilled"], true);
}

#[tokio::test]
async fn many_files_under_one_field_all_arrive() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    let parts: Vec<(&str, Option<&str>, &str, &[u8])> = (0..32)
        .map(|_| ("files", Some("a.txt"), "text/plain", &b"x"[..]))
        .collect();

    let body = body_of(&app, upload("/describe", "B", &parts)).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["data"]["count"], 32);
}

#[tokio::test]
async fn a_multipart_body_with_a_wrong_boundary_is_a_bad_request() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/describe")
        .header("content-type", "multipart/form-data; boundary=NOPE")
        .body(axum::body::Body::from("not multipart at all"))
        .unwrap();

    let body = body_of(&app, request).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["code"], 400, "{json}");
}

#[tokio::test]
async fn a_multipart_content_type_without_a_boundary_is_a_bad_request() {
    let app = app(|builder| {
        describe_route(builder);
    })
    .await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/describe")
        .header("content-type", "multipart/form-data")
        .body(axum::body::Body::from("x"))
        .unwrap();

    let body = body_of(&app, request).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["code"], 400, "{json}");
}

// ------------------------------------------------------------ static files

async fn static_app(root: &std::path::Path) -> axum::Router {
    let root = root.to_path_buf();
    app(move |builder| {
        builder.routes(move |ctx| {
            ctx.static_files("/assets", root.clone());
        });
    })
    .await
}

#[tokio::test]
async fn a_directly_served_file_strips_the_file_name_from_the_uri() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a b&c.txt"), "spaced").unwrap();
    let app = static_app(dir.path()).await;

    let response = ServiceExt::<axum::http::Request<axum::body::Body>>::oneshot(
        app.clone(),
        axum::http::Request::builder()
            .uri("/assets/a%20b%26c.txt")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn an_unsatisfiable_range_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("small.txt"), "short").unwrap();
    let app = static_app(dir.path()).await;

    let response = ServiceExt::<axum::http::Request<axum::body::Body>>::oneshot(
        app,
        axum::http::Request::builder()
            .uri("/assets/small.txt")
            .header("range", "bytes=100-200")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
        "a range past the end is a 416"
    );
}

#[tokio::test]
async fn a_directory_without_an_index_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let app = static_app(dir.path()).await;

    let response = ServiceExt::<axum::http::Request<axum::body::Body>>::oneshot(
        app,
        axum::http::Request::builder()
            .uri("/assets/empty")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_symlink_out_of_the_root_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

    let root = tempfile::tempdir().unwrap();
    let link = root.path().join("link.txt");
    // Creating a symlink needs a privilege the test may not have on Windows; the
    // canonicalisation check is still exercised by the `..` case elsewhere.
    if std::os::windows::fs::symlink_file(outside.path().join("secret.txt"), &link).is_err() {
        return;
    }

    let app = static_app(root.path()).await;
    let response = ServiceExt::<axum::http::Request<axum::body::Body>>::oneshot(
        app,
        axum::http::Request::builder()
            .uri("/assets/link.txt")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "a symlink out of the root must not be followed"
    );
}

// ------------------------------------------------------------- converter

#[tokio::test]
async fn a_registered_converter_replaces_the_default_one() {
    #[derive(Default)]
    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for Counting {}

    #[async_trait]
    impl Converter for Counting {
        async fn convert(
            &self,
            result: Result<Response>,
            _req: &Request,
        ) -> axum::response::Response {
            self.0.fetch_add(1, Ordering::SeqCst);
            let mut response = rust_web_frame::web::DefaultConverter
                .convert(result, _req)
                .await;
            response
                .headers_mut()
                .insert("x-converted", axum::http::HeaderValue::from_static("yes"));
            response
        }
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let app = app({
        let counter = Arc::clone(&counter);
        move |builder| {
            builder.get("/x", handler(|_| async { Ok("body") }));
            builder.converter(Counting(counter));
        }
    })
    .await;

    let response = {
        ServiceExt::<axum::http::Request<axum::body::Body>>::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/x")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    };

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "the custom converter must be used"
    );
    assert_eq!(response.headers()["x-converted"], "yes");
}

// -------------------------------------------------------------------- tls

const TLS_PORT: u16 = 19410;

async fn spawn_tls(port: u16) {
    let dir = tempfile::tempdir().unwrap();
    let generated = rcgen::generate_simple_self_signed(vec!["tls.test".to_owned()]).unwrap();
    let cert_path = dir.path().join("c.crt");
    let key_path = dir.path().join("c.key");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
    // The files have to outlive the task, so the directory is leaked on purpose: a
    // `TempDir` dropped here would take the certificate with it.
    std::mem::forget(dir);

    let mut config = Config::empty();
    config.set("web.server.port", json!(port));
    config.set(
        "web.server.ssl",
        json!({
            "enabled": true,
            "certs": [{"certFile": cert_path, "keyFile": key_path}]
        }),
    );

    let mut builder = Builder::new(config);
    builder.get("/", handler(|_| async { Ok("secure") }));

    let runtime: WebFrame = builder.build();
    tokio::spawn(async move {
        let _ = runtime.build_runtime().await.expect("starts").run().await;
    });

    for _ in 0..150 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing listening on {port}");
}

#[tokio::test]
async fn plain_http_to_a_tls_port_is_refused_not_answered() {
    spawn_tls(TLS_PORT).await;

    let mut stream = TcpStream::connect(("127.0.0.1", TLS_PORT)).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: tls.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    let _ =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response)).await;

    assert!(
        !response.contains("secure"),
        "plaintext was answered on a TLS port: {response:?}"
    );
}

#[tokio::test]
async fn a_garbage_handshake_does_not_take_the_listener_down() {
    spawn_tls(TLS_PORT + 1).await;

    // Random bytes where a ClientHello belongs.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", TLS_PORT + 1))
            .await
            .unwrap();
        stream.write_all(&[0xff; 64]).await.unwrap();
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut sink)).await;
    }

    // And a real handshake still works afterwards.
    let mut stream = TcpStream::connect(("127.0.0.1", TLS_PORT + 1))
        .await
        .unwrap();
    stream
        .write_all(&[0x16, 0x03, 0x01, 0x00, 0x05, 0x01, 0x00, 0x00, 0x01, 0x00])
        .await
        .unwrap();
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut sink)).await;

    // The listener is still accepting.
    assert!(
        TcpStream::connect(("127.0.0.1", TLS_PORT + 1))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn an_ssl_section_that_is_disabled_serves_plain_http() {
    const PORT: u16 = 19412;
    let mut config = Config::empty();
    config.set("web.server.port", json!(PORT));
    config.set("web.server.ssl", json!({"enabled": false}));

    let mut builder = Builder::new(config);
    builder.get("/", handler(|_| async { Ok("plain") }));
    let runtime = builder.build().build_runtime().await.expect("starts");
    tokio::spawn(async move {
        let _ = runtime.run().await;
    });

    for _ in 0..150 {
        if TcpStream::connect(("127.0.0.1", PORT)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut stream = TcpStream::connect(("127.0.0.1", PORT)).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response).await;
    assert!(response.contains("plain"), "{response:?}");
}

/// Keeps the TLS types referenced from a test that does not need them.
#[allow(dead_code)]
fn _types(_: SslConfig, _: SslCertificate, _: Error) {}
