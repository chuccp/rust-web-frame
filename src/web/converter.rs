//! Turns a handler's result into an HTTP response.

use std::convert::Infallible;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::IntoResponse as _;
use axum::response::sse::{KeepAlive, Sse};
use futures_util::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::error::{Error, Result, code};
use super::file::FileResponse;
use super::message::Message;
use super::request::Request;
use super::response::{Response, ResponseBody};
use super::sse::SseStream;
use super::ws::WebSocketStream;

/// Transforms the filter chain's result into an HTTP response.
///
/// Implement this to change how *every* response is rendered — a different envelope, an
/// XML serialiser, field encryption.
///
/// ```no_run
/// # use rust_web_frame::web::{Converter, Request, Response, Result};
/// # use async_trait::async_trait;
/// struct LoggingConverter;
///
/// #[async_trait]
/// impl Converter for LoggingConverter {
///     async fn convert(&self, result: Result<Response>, req: &Request) -> axum::response::Response {
///         let response = rust_web_frame::web::DefaultConverter.convert(result, req).await;
///         tracing::info!(path = req.path(), status = response.status().as_u16(), "response");
///         response
///     }
/// }
/// ```
#[async_trait]
pub trait Converter: Send + Sync + 'static {
    /// Renders `result` for `req`.
    async fn convert(&self, result: Result<Response>, req: &Request) -> axum::response::Response;
}

/// The built-in converter: JSON envelopes, plain text, downloads, redirects, SSE and
/// WebSocket upgrades.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultConverter;

#[async_trait]
impl Converter for DefaultConverter {
    async fn convert(&self, result: Result<Response>, req: &Request) -> axum::response::Response {
        let mut response = match result {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(code = error.code, error = %error, "handler failed");
                Response::error(error)
            }
        };

        // Cookies queued by the handler travel on every kind of response.
        let cookies = req.take_cookies_to_set();
        for cookie in cookies {
            response.try_set_header("set-cookie", cookie.to_header_value());
        }

        let status = response.status();
        let (headers, _, body) = response.into_parts();

        let mut built = match body {
            ResponseBody::Empty => axum::response::Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap_or_else(|_| fallback_response()),
            ResponseBody::Text(text) => axum::response::Response::builder()
                .status(status)
                .header("content-type", "text/plain; charset=utf-8")
                .body(Body::from(text))
                .unwrap_or_else(|_| fallback_response()),
            ResponseBody::Json(value) => axum::response::Response::builder()
                .status(status)
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(value.to_string()))
                .unwrap_or_else(|_| fallback_response()),
            ResponseBody::Message(message) => json_response(status, &message),
            ResponseBody::Error(error) => json_response(status, &error_envelope(&error)),
            ResponseBody::Redirect(location) => axum::response::Response::builder()
                .status(status)
                .header("location", location)
                .body(Body::empty())
                .unwrap_or_else(|_| fallback_response()),
            ResponseBody::File(file) => render_file(&file).await,
            ResponseBody::Sse(handler) => render_sse(handler),
            ResponseBody::WebSocket(handler) => render_websocket(handler, req),
            ResponseBody::Raw(response) => response,
        };

        merge_headers(built.headers_mut(), headers);
        built
    }
}

fn fallback_response() -> axum::response::Response {
    axum::response::Response::builder()
        .status(500)
        .body(Body::empty())
        .expect("a static response always builds")
}

fn json_response(status: axum::http::StatusCode, message: &Message) -> axum::response::Response {
    match serde_json::to_string(message) {
        Ok(json) => axum::response::Response::builder()
            .status(status)
            .header("content-type", "application/json; charset=utf-8")
            .body(Body::from(json))
            .unwrap_or_else(|_| fallback_response()),
        Err(err) => {
            tracing::error!(error = %err, "response envelope is not serialisable");
            fallback_response()
        }
    }
}

/// Builds the envelope sent for an [`Error`].
pub fn error_envelope(error: &Error) -> Message {
    Message {
        code: error.code,
        data: serde_json::Value::Null,
        msg: error.client_message(),
        ty: String::new(),
    }
}

async fn render_file(file: &FileResponse) -> axum::response::Response {
    match tokio::fs::read(file.path()).await {
        Ok(bytes) => axum::response::Response::builder()
            .status(200)
            .header("content-disposition", file.content_disposition())
            .header("content-type", content_type_for(&file.resolved_file_name()))
            .body(Body::from(bytes))
            .unwrap_or_else(|_| fallback_response()),
        Err(err) => {
            let error = Error::from(err);
            let status = axum::http::StatusCode::from_u16(error.http_status())
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            json_response(status, &error_envelope(&error))
        }
    }
}

/// Guesses a `Content-Type` from a file name.
#[must_use]
pub fn content_type_for(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("txt" | "log") => "text/plain; charset=utf-8",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("zip") => "application/zip",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}

fn render_sse(handler: super::sse::SseHandler) -> axum::response::Response {
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let stream = SseStream::new(sender);
    let probe = stream.clone();

    tokio::spawn(async move {
        if let Err(err) = handler(stream).await {
            tracing::debug!(error = %err, "sse handler finished with an error");
        }
        probe.close();
    });

    let events = ReceiverStream::new(receiver).map(Ok::<_, Infallible>);
    Sse::new(events)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("heartbeat"),
        )
        .into_response()
}

fn render_websocket(
    handler: super::ws::WebSocketHandler,
    req: &Request,
) -> axum::response::Response {
    match req.take_websocket_upgrade() {
        Some(upgrade) => upgrade.on_upgrade(move |socket| async move {
            let stream = WebSocketStream::new(socket);
            if let Err(err) = handler(stream).await {
                tracing::debug!(error = %err, "websocket handler finished with an error");
            }
        }),
        None => {
            let error = Error::new(code::BAD_REQUEST, "expected a websocket upgrade");
            json_response(axum::http::StatusCode::BAD_REQUEST, &error_envelope(&error))
        }
    }
}

fn merge_headers(target: &mut axum::http::HeaderMap, extra: axum::http::HeaderMap) {
    for (name, value) in extra {
        if let Some(name) = name {
            target.insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{Response, handler};
    use axum::body::to_bytes;

    #[tokio::test]
    async fn message_bodies_are_wrapped_in_the_envelope() {
        let response = Response::message(Message::data(serde_json::json!({"id": 1})));
        let built = DefaultConverter
            .convert(Ok(response), &Request::for_test("/"))
            .await;
        assert_eq!(built.status(), axum::http::StatusCode::OK);

        let body = to_bytes(built.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], 200);
        assert_eq!(json["data"]["id"], 1);
    }

    #[tokio::test]
    async fn raw_json_bodies_are_sent_untouched() {
        let built = DefaultConverter
            .convert(
                Ok(Response::json(serde_json::json!({"id": 1}))),
                &Request::for_test("/"),
            )
            .await;
        assert_eq!(built.status(), axum::http::StatusCode::OK);

        let body = to_bytes(built.into_body(), 1 << 20).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"id": 1})
        );
    }

    #[tokio::test]
    async fn errors_map_to_their_status_code() {
        let built = DefaultConverter
            .convert(Err(Error::not_found()), &Request::for_test("/"))
            .await;
        assert_eq!(built.status(), axum::http::StatusCode::NOT_FOUND);

        let body = to_bytes(built.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], 404);
        assert_eq!(json["msg"], "not found");
    }

    #[tokio::test]
    async fn business_errors_are_sent_over_http_200() {
        let built = DefaultConverter
            .convert(Err(Error::token_expired()), &Request::for_test("/"))
            .await;
        assert_eq!(built.status(), axum::http::StatusCode::OK);

        let body = to_bytes(built.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], 1003);
    }

    #[tokio::test]
    async fn queued_cookies_are_written_to_the_response() {
        let req = Request::for_test("/");
        req.set_cookie("token", "abc", 3600);
        let built = DefaultConverter
            .convert(Ok(Response::text("ok")), &req)
            .await;
        let cookie = built.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("token=abc"));
        assert!(cookie.contains("Max-Age=3600"));
    }

    #[tokio::test]
    async fn raw_responses_pass_through_untouched() {
        let raw = axum::response::Response::builder()
            .status(418)
            .header("x-tea", "pot")
            .body(Body::from("short and stout"))
            .unwrap();
        let built = DefaultConverter
            .convert(Ok(Response::raw(raw)), &Request::for_test("/"))
            .await;
        assert_eq!(built.status().as_u16(), 418);
        assert_eq!(built.headers().get("x-tea").unwrap(), "pot");
    }

    #[tokio::test]
    async fn redirects_carry_a_location_header() {
        let built = DefaultConverter
            .convert(Ok(Response::redirect("/new")), &Request::for_test("/"))
            .await;
        assert_eq!(built.status().as_u16(), 301);
        assert_eq!(built.headers().get("location").unwrap(), "/new");
    }

    #[test]
    fn handlers_can_return_raw_json_values() {
        let _ = handler(|_req| async { Ok(serde_json::json!({"ok": true})) });
        let _ = handler(|_req| async { Ok("plain") });
        let _ = handler(|_req| async { Ok(Message::ok()) });
    }
}
