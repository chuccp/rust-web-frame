//! The framework's response type and the `IntoResponse` conversion used by handlers.

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::{Value, json};

use super::error::{Error, code};
use super::file::FileResponse;
use super::message::Message;
use super::sse::SseHandler;
use super::ws::WebSocketHandler;

/// The body of a [`Response`].
pub enum ResponseBody {
    /// No body at all.
    Empty,
    /// `text/plain`.
    Text(String),
    /// A raw JSON value, sent as-is (not wrapped in a [`Message`] envelope).
    Json(Value),
    /// The standard [`Message`] envelope.
    Message(Box<Message>),
    /// An error; the converter turns it into a `Message` with the right status code.
    Error(Box<Error>),
    /// A `301` redirect.
    Redirect(String),
    /// A file download.
    File(Box<FileResponse>),
    /// A Server-Sent Events stream.
    Sse(SseHandler),
    /// A WebSocket upgrade.
    WebSocket(WebSocketHandler),
    /// An already-built axum response, passed through untouched.
    Raw(axum::response::Response),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Text(text) => f.debug_tuple("Text").field(text).finish(),
            Self::Json(value) => f.debug_tuple("Json").field(value).finish(),
            Self::Message(message) => f.debug_tuple("Message").field(message).finish(),
            Self::Error(error) => f.debug_tuple("Error").field(error).finish(),
            Self::Redirect(location) => f.debug_tuple("Redirect").field(location).finish(),
            Self::File(file) => f.debug_tuple("File").field(file).finish(),
            Self::Sse(_) => f.write_str("Sse(..)"),
            Self::WebSocket(_) => f.write_str("WebSocket(..)"),
            Self::Raw(response) => f.debug_tuple("Raw").field(&response.status()).finish(),
        }
    }
}

/// What a handler gives back: a status, a set of headers and a body.
///
/// A filter can post-process any response, whatever its body:
///
/// ```no_run
/// # use rust_web_frame::web::{Response, Result};
/// fn tag(response: Response) -> Result<Response> {
///     Ok(response.with_header("x-served-by", "rust-web-frame"))
/// }
/// ```
pub struct Response {
    pub(crate) headers: HeaderMap,
    pub(crate) status_override: Option<StatusCode>,
    pub(crate) body: ResponseBody,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status())
            .field("headers", &self.headers)
            .field("body", &self.body)
            .finish()
    }
}

impl Response {
    /// Builds a response from a body.
    pub fn new(body: ResponseBody) -> Self {
        Self {
            headers: HeaderMap::new(),
            status_override: None,
            body,
        }
    }

    /// `200 OK` with no body.
    pub fn empty() -> Self {
        Self::new(ResponseBody::Empty)
    }

    /// `text/plain`.
    pub fn text(body: impl Into<String>) -> Self {
        Self::new(ResponseBody::Text(body.into()))
    }

    /// A raw JSON value, **not** wrapped in a [`Message`].
    pub fn json(body: impl Into<Value>) -> Self {
        Self::new(ResponseBody::Json(body.into()))
    }

    /// The standard envelope.
    pub fn message(message: Message) -> Self {
        Self::new(ResponseBody::Message(Box::new(message)))
    }

    /// An error response. The converter maps it to the right status code and envelope.
    pub fn error(error: Error) -> Self {
        Self::new(ResponseBody::Error(Box::new(error)))
    }

    /// A `301` redirect.
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::new(ResponseBody::Redirect(location.into()))
    }

    /// A file download.
    pub fn file(file: FileResponse) -> Self {
        Self::new(ResponseBody::File(Box::new(file)))
    }

    /// Wraps an already-built axum response.
    pub fn raw(response: axum::response::Response) -> Self {
        Self::new(ResponseBody::Raw(response))
    }

    /// Forces a status code, overriding the one implied by the body.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status_override = StatusCode::from_u16(status).ok();
        self
    }

    /// Adds a header, consuming the response so calls chain.
    ///
    /// Invalid header names or values are ignored; use [`set_header`](Self::set_header)
    /// when you would rather see the response anyway.
    #[must_use]
    pub fn with_header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.set_header(name, value);
        self
    }

    /// Adds a header in place, ignoring invalid names or values.
    pub fn set_header(&mut self, name: impl AsRef<str>, value: impl AsRef<str>) {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_ref()),
            HeaderValue::try_from(value.as_ref()),
        ) {
            self.headers.insert(name, value);
        }
    }

    /// Reads a header previously set on this response.
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// The explicit status override, if any.
    pub fn status_override(&self) -> Option<StatusCode> {
        self.status_override
    }

    /// The response body.
    pub fn body(&self) -> &ResponseBody {
        &self.body
    }

    /// Consumes the response, yielding its parts.
    pub fn into_parts(self) -> (HeaderMap, Option<StatusCode>, ResponseBody) {
        (self.headers, self.status_override, self.body)
    }

    /// The status code this response will be sent with.
    pub fn status(&self) -> StatusCode {
        if let Some(status) = self.status_override {
            return status;
        }
        match &self.body {
            ResponseBody::Empty | ResponseBody::Text(_) | ResponseBody::Json(_) => StatusCode::OK,
            ResponseBody::Message(message) => {
                StatusCode::from_u16(code::http_status(message.code)).unwrap_or(StatusCode::OK)
            }
            ResponseBody::Error(error) => StatusCode::from_u16(error.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            ResponseBody::Redirect(_) => StatusCode::MOVED_PERMANENTLY,
            ResponseBody::File(_) | ResponseBody::Sse(_) | ResponseBody::WebSocket(_) => {
                StatusCode::OK
            }
            ResponseBody::Raw(response) => response.status(),
        }
    }
}

/// Conversion from a handler's return value into a [`Response`].
///
/// Implemented for the types handlers actually return, so `Ok("hello")`,
/// `Ok(json!({...}))` and `Ok(Message::ok())` all work without ceremony.
pub trait IntoResponse: Send + 'static {
    /// Converts `self` into a response.
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for ResponseBody {
    fn into_response(self) -> Response {
        Response::new(self)
    }
}

impl IntoResponse for Message {
    fn into_response(self) -> Response {
        Response::message(self)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        Response::error(self)
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for Value {
    fn into_response(self) -> Response {
        Response::json(self)
    }
}

impl IntoResponse for FileResponse {
    fn into_response(self) -> Response {
        Response::file(self)
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::message(Message::ok())
    }
}

impl IntoResponse for bool {
    fn into_response(self) -> Response {
        Response::json(json!(self))
    }
}

impl IntoResponse for serde_json::Map<String, Value> {
    fn into_response(self) -> Response {
        Response::json(Value::Object(self))
    }
}

macro_rules! into_response_for_numbers {
    ($($ty:ty),*) => {
        $(
            impl IntoResponse for $ty {
                fn into_response(self) -> Response {
                    Response::json(json!(self))
                }
            }
        )*
    };
}

into_response_for_numbers!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_body() {
        assert_eq!(Response::text("hi").status(), StatusCode::OK);
        assert_eq!(
            Response::error(Error::not_found()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            Response::redirect("/x").status(),
            StatusCode::MOVED_PERMANENTLY
        );
        assert_eq!(
            Response::message(Message::with_code(1003, json!(null))).status(),
            StatusCode::OK
        );
    }

    #[test]
    fn explicit_status_wins() {
        let response = Response::text("teapot").with_status(418);
        assert_eq!(response.status().as_u16(), 418);
    }

    #[test]
    fn into_response_covers_the_common_handler_returns() {
        assert!(matches!("hi".into_response().body(), ResponseBody::Text(_)));
        assert!(matches!(
            ().into_response().body(),
            ResponseBody::Message(_)
        ));
        assert!(matches!(
            json!({"a":1}).into_response().body(),
            ResponseBody::Json(_)
        ));
        assert!(matches!(
            Error::forbidden().into_response().body(),
            ResponseBody::Error(_)
        ));
    }
}
