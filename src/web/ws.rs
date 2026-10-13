//! WebSocket endpoints.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};

use super::error::{Error, Result};
use super::filter::BoxFuture;

/// The user's WebSocket endpoint: it owns the connection until it returns.
///
/// ```no_run
/// # use rust_web_frame::web::{WebSocketStream, Result};
/// async fn echo(mut socket: WebSocketStream) -> Result<()> {
///     while let Some(frame) = socket.read().await {
///         if frame.is_text() {
///             socket.write_text(&frame.text()).await?;
///         }
///     }
///     Ok(()) // client disconnected
/// }
/// ```
pub type WebSocketHandler = Arc<dyn Fn(WebSocketStream) -> BoxFuture<Result<()>> + Send + Sync>;

/// The kind of frame a client sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// A UTF-8 text frame.
    Text,
    /// A binary frame.
    Binary,
    /// A ping.
    Ping,
    /// A pong.
    Pong,
    /// A close frame.
    Close,
}

/// A received frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// What kind of frame this is.
    pub typ: MessageType,
    /// The raw payload.
    pub data: Vec<u8>,
}

impl Frame {
    /// The payload decoded as UTF-8, lossily.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// Reports whether this is a text frame.
    pub fn is_text(&self) -> bool {
        self.typ == MessageType::Text
    }

    /// Reports whether this is a binary frame.
    pub fn is_binary(&self) -> bool {
        self.typ == MessageType::Binary
    }

    /// Reports whether this is a close frame.
    pub fn is_close(&self) -> bool {
        self.typ == MessageType::Close
    }
}

/// A live WebSocket connection.
pub struct WebSocketStream {
    socket: WebSocket,
}

impl WebSocketStream {
    pub(crate) fn new(socket: WebSocket) -> Self {
        Self { socket }
    }

    /// Reads the next frame. Returns `None` once the client disconnects or the protocol
    /// fails.
    ///
    /// Ping and pong frames are surfaced to the caller; axum answers protocol-level
    /// keepalives on its own.
    pub async fn read(&mut self) -> Option<Frame> {
        match self.socket.recv().await {
            Some(Ok(Message::Text(text))) => Some(Frame {
                typ: MessageType::Text,
                data: text.as_str().as_bytes().to_vec(),
            }),
            Some(Ok(Message::Binary(data))) => Some(Frame {
                typ: MessageType::Binary,
                data: data.to_vec(),
            }),
            Some(Ok(Message::Ping(data))) => Some(Frame {
                typ: MessageType::Ping,
                data: data.to_vec(),
            }),
            Some(Ok(Message::Pong(data))) => Some(Frame {
                typ: MessageType::Pong,
                data: data.to_vec(),
            }),
            Some(Ok(Message::Close(frame))) => Some(Frame {
                typ: MessageType::Close,
                data: frame
                    .map(|f| f.reason.as_str().as_bytes().to_vec())
                    .unwrap_or_default(),
            }),
            Some(Err(err)) => {
                tracing::debug!(error = %err, "websocket receive failed");
                None
            }
            None => None,
        }
    }

    /// Reads the next frame and requires it to be text.
    pub async fn read_text(&mut self) -> Result<String> {
        match self.read().await {
            Some(frame) if frame.is_text() => Ok(frame.text()),
            Some(frame) => {
                Err(Error::bad_request()
                    .detail(format!("expected a text frame, got {:?}", frame.typ)))
            }
            None => Err(Error::bad_request().detail("connection closed")),
        }
    }

    /// Reads the next frame and requires it to be binary.
    pub async fn read_binary(&mut self) -> Result<Vec<u8>> {
        match self.read().await {
            Some(frame) if frame.is_binary() => Ok(frame.data),
            Some(frame) => Err(Error::bad_request()
                .detail(format!("expected a binary frame, got {:?}", frame.typ))),
            None => Err(Error::bad_request().detail("connection closed")),
        }
    }

    /// Sends a text frame.
    pub async fn write_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.socket
            .send(Message::Text(text.into().into()))
            .await
            .map_err(|err| Error::with_source(500, err))
    }

    /// Sends a binary frame.
    pub async fn write_binary(&mut self, data: impl Into<Vec<u8>>) -> Result<()> {
        self.socket
            .send(Message::Binary(data.into().into()))
            .await
            .map_err(|err| Error::with_source(500, err))
    }

    /// Sends a frame of an explicit type. Close and ping frames are handled by
    /// [`close`](Self::close) and [`ping`](Self::ping).
    pub async fn write(&mut self, typ: MessageType, data: &[u8]) -> Result<()> {
        match typ {
            MessageType::Text => {
                self.write_text(String::from_utf8_lossy(data).into_owned())
                    .await
            }
            MessageType::Binary => self.write_binary(data.to_vec()).await,
            MessageType::Ping => self.ping(data).await,
            MessageType::Pong => self
                .socket
                .send(Message::Pong(data.to_vec().into()))
                .await
                .map_err(|err| Error::with_source(500, err)),
            MessageType::Close => {
                self.close().await;
                Ok(())
            }
        }
    }

    /// Sends a ping.
    pub async fn ping(&mut self, data: &[u8]) -> Result<()> {
        self.socket
            .send(Message::Ping(data.to_vec().into()))
            .await
            .map_err(|err| Error::with_source(500, err))
    }

    /// Closes the connection with a normal closure.
    pub async fn close(&mut self) {
        let _ = self.socket.send(Message::Close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_expose_their_payload() {
        let frame = Frame {
            typ: MessageType::Text,
            data: b"hello".to_vec(),
        };
        assert!(frame.is_text());
        assert!(!frame.is_binary());
        assert_eq!(frame.text(), "hello");
    }

    #[test]
    fn non_utf8_text_frames_decode_lossily() {
        let frame = Frame {
            typ: MessageType::Text,
            data: vec![0xff, 0xfe],
        };
        assert!(!frame.text().is_empty());
    }
}
