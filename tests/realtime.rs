//! SSE and WebSocket, driven over a real socket.
//!
//! The router-level tests in `web_layer.rs` can exercise a request/response pair, but a
//! connection that stays open needs a listener, so these tests start one on a fixed port.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rust_web_frame::Builder;
use rust_web_frame::config::Config;
use rust_web_frame::web::{Result, SseStream, WebSocketStream, handler};
use tokio_tungstenite::tungstenite::Message;

/// Ports the tests bind. Fixed because the runtime reports the configured port, not the
/// bound one, and an ephemeral port would need plumbing to discover.
const SSE_PORT: u16 = 19181;
const WS_PORT: u16 = 19182;
const SHUTDOWN_PORT: u16 = 19183;

async fn spawn(port: u16, configure: impl FnOnce(&mut Builder)) {
    let mut config = Config::empty();
    config.set("web.server.port", serde_json::json!(port));

    let mut builder = Builder::new(config);
    configure(&mut builder);

    // The server runs until the process exits; the tests are done before that.
    tokio::spawn(async move {
        let runtime = builder.build().build_runtime().await.expect("starts");
        let _ = runtime.run().await;
    });

    wait_for_port(port).await;
}

async fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing started listening on {port}");
}

// ------------------------------------------------------------------ websocket

#[tokio::test]
async fn a_websocket_endpoint_echoes() {
    spawn(WS_PORT, |builder| {
        builder.routes(|ctx| {
            ctx.websocket("/ws", |mut socket: WebSocketStream| {
                Box::pin(async move {
                    while let Some(frame) = socket.read().await {
                        if frame.is_text() {
                            let text = frame.text();
                            if text == "bye" {
                                socket.close().await;
                                break;
                            }
                            socket.write_text(format!("echo:{text}")).await?;
                        } else if frame.is_close() {
                            break;
                        }
                    }
                    Ok(())
                })
            });
        });
    })
    .await;

    let (mut socket, response) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{WS_PORT}/ws"))
            .await
            .expect("the upgrade is accepted");
    assert_eq!(response.status().as_u16(), 101, "101 Switching Protocols");

    socket
        .send(Message::Text("hello".into()))
        .await
        .expect("send");
    let reply = socket.next().await.expect("a frame").expect("no error");
    assert_eq!(reply, Message::Text("echo:hello".into()));

    socket
        .send(Message::Text("again".into()))
        .await
        .expect("send");
    let reply = socket.next().await.expect("a frame").expect("no error");
    assert_eq!(reply, Message::Text("echo:again".into()));

    socket.send(Message::Close(None)).await.expect("close");
}

#[tokio::test]
async fn a_binary_frame_survives_the_round_trip() {
    spawn(WS_PORT + 10, |builder| {
        builder.routes(|ctx| {
            ctx.websocket("/bin", |mut socket: WebSocketStream| {
                Box::pin(async move {
                    while let Some(frame) = socket.read().await {
                        if frame.is_binary() {
                            socket.write_binary(frame.data).await?;
                        } else if frame.is_close() {
                            break;
                        }
                    }
                    Ok(())
                })
            });
        });
    })
    .await;

    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/bin", WS_PORT + 10))
            .await
            .expect("connect");

    let payload: Vec<u8> = (0..=255u8).collect();
    socket
        .send(Message::Binary(payload.clone().into()))
        .await
        .expect("send");
    let reply = socket.next().await.expect("a frame").expect("no error");
    match reply {
        Message::Binary(data) => assert_eq!(data.as_ref(), payload.as_slice()),
        other => panic!("expected binary, got {other:?}"),
    }
}

// ------------------------------------------------------------------------ sse

#[tokio::test]
async fn an_sse_endpoint_streams_named_events() {
    spawn(SSE_PORT, |builder| {
        builder.routes(|ctx| {
            ctx.sse("/events", |stream: SseStream| {
                Box::pin(async move {
                    stream.send("tick", "1");
                    stream.send_message("plain");
                    stream.send_with_id("7", "tick", "3");
                    stream.send_json("json", &serde_json::json!({"a": 1}));
                    Ok(())
                })
            });
        });
    })
    .await;

    let body = reqwest_get(format!("http://127.0.0.1:{SSE_PORT}/events")).await;

    assert!(
        body.contains("event: tick\ndata: 1"),
        "named event missing:\n{body}"
    );
    assert!(
        body.contains("data: plain"),
        "data-only event missing:\n{body}"
    );
    assert!(body.contains("id: 7"), "event id missing:\n{body}");
    assert!(
        body.contains(r#"data: {"a":1}"#),
        "json event missing:\n{body}"
    );
}

/// A minimal HTTP/1.1 GET, so the test does not need an HTTP client dependency for one
/// request.
async fn reqwest_get(url: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let url = url.strip_prefix("http://").expect("http url");
    let (authority, path) = url.split_once('/').unwrap_or((url, ""));
    let mut stream = tokio::net::TcpStream::connect(authority)
        .await
        .expect("connect");
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("write");

    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or(response)
}

// ------------------------------------------------------------------- shutdown

#[tokio::test]
async fn asking_the_context_to_stop_ends_the_run() {
    let mut config = Config::empty();
    config.set("web.server.port", serde_json::json!(SHUTDOWN_PORT));

    let stop = std::sync::Arc::new(std::sync::OnceLock::<rust_web_frame::core::Context>::new());
    let handle = stop.clone();

    let mut builder = Builder::new(config);
    builder.routes(move |ctx| {
        let _ = handle.set(ctx.clone());
        let owned = ctx.clone();
        ctx.get(
            "/quit",
            handler(move |_req: rust_web_frame::web::Request| {
                let ctx = owned.clone();
                async move {
                    ctx.shutdown();
                    Ok("stopping")
                }
            }),
        );
    });

    let runtime = builder.build().build_runtime().await.expect("starts");
    let run = tokio::spawn(async move { runtime.run().await });

    wait_for_port(SHUTDOWN_PORT).await;
    let body = reqwest_get(format!("http://127.0.0.1:{SHUTDOWN_PORT}/quit")).await;
    assert!(body.contains("stopping"), "got {body}");

    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("run() must return once the context is cancelled")
        .expect("the task did not panic");
    assert!(outcome.is_ok(), "shutdown is not a failure: {outcome:?}");
}

/// Keeps `Result` in scope for the handler signatures above.
#[allow(dead_code)]
fn _assert_result_alias(_: Result<()>) {}
