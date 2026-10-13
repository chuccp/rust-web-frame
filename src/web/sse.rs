//! Server-Sent Events.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::response::sse::Event;
use tokio::sync::mpsc;

use super::error::Result;
use super::filter::BoxFuture;

/// The user's SSE endpoint: it is handed a live [`SseStream`] and pushes events at it
/// until it returns.
///
/// ```no_run
/// # use rust_web_frame::web::{SseStream, Result};
/// # use std::time::Duration;
/// async fn ticks(stream: SseStream) -> Result<()> {
///     for i in 0..10 {
///         stream.send("tick", i.to_string());
///         tokio::time::sleep(Duration::from_secs(1)).await;
///     }
///     Ok(())
/// }
/// ```
pub type SseHandler = Arc<dyn Fn(SseStream) -> BoxFuture<Result<()>> + Send + Sync>;

/// The write half of a Server-Sent Events connection.
///
/// The stream is closed when the handler returns, when the client disconnects, or when
/// [`close`](Self::close) is called.
#[derive(Clone)]
pub struct SseStream {
    sender: mpsc::Sender<Event>,
    closed: Arc<AtomicBool>,
}

impl SseStream {
    pub(crate) fn new(sender: mpsc::Sender<Event>) -> Self {
        Self {
            sender,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sends a named event. Clients receive it via `addEventListener(name, ...)`.
    pub fn send(&self, event: impl Into<String>, data: impl Into<String>) {
        self.try_send(Event::default().event(event.into()).data(data.into()));
    }

    /// Sends a data-only event, received as the generic `message` event.
    pub fn send_message(&self, data: impl Into<String>) {
        self.try_send(Event::default().data(data.into()));
    }

    /// Sends an event carrying an `id`, which a reconnecting client echoes back in the
    /// `Last-Event-ID` header.
    pub fn send_with_id(
        &self,
        id: impl Into<String>,
        event: impl Into<String>,
        data: impl Into<String>,
    ) {
        self.try_send(
            Event::default()
                .id(id.into())
                .event(event.into())
                .data(data.into()),
        );
    }

    /// Sends a JSON payload as a named event.
    pub fn send_json(&self, event: impl Into<String>, data: &impl serde::Serialize) {
        match serde_json::to_string(data) {
            Ok(json) => self.send(event, json),
            Err(err) => {
                tracing::warn!(error = %err, "sse: payload is not serialisable, event dropped")
            }
        }
    }

    /// Tells the client how long to wait before reconnecting, in milliseconds.
    pub fn send_retry(&self, millis: u64) {
        self.try_send(Event::default().retry(Duration::from_millis(millis)));
    }

    /// Sends a comment, which keeps proxies from closing an idle connection.
    pub fn heartbeat(&self) {
        self.try_send(Event::default().comment("heartbeat"));
    }

    /// Spawns a task that emits a heartbeat every `interval` until the stream closes.
    pub fn start_heartbeat(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let stream = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                if stream.is_closed() {
                    return;
                }
                stream.heartbeat();
            }
        })
    }

    /// Closes the stream. The client sees the connection end.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Reports whether the stream is finished — the client is gone, or
    /// [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn try_send(&self, event: Event) {
        if self.is_closed() {
            return;
        }
        if self.sender.try_send(event).is_err() {
            // The client is not draining the connection; stop pretending it is there.
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    /// Awaits until the client disconnects or the stream is closed.
    pub async fn done(&self) {
        while !self.is_closed() {
            if self.sender.is_closed() {
                self.closed.store(true, Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_reach_the_receiver_in_order() {
        let (tx, mut rx) = mpsc::channel(8);
        let stream = SseStream::new(tx);

        stream.send("tick", "1");
        stream.send_message("plain");
        stream.send_with_id("7", "tick", "2");

        let first = rx.recv().await.unwrap();
        assert!(format!("{first:?}").contains("tick"));
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn a_closed_stream_drops_events() {
        let (tx, mut rx) = mpsc::channel(8);
        let stream = SseStream::new(tx);
        stream.close();
        stream.send("tick", "1");
        assert!(rx.try_recv().is_err());
        assert!(stream.is_closed());
    }

    #[tokio::test]
    async fn a_full_buffer_marks_the_stream_closed() {
        let (tx, _rx) = mpsc::channel(1);
        let stream = SseStream::new(tx);
        stream.send("tick", "1");
        stream.send("tick", "2");
        assert!(stream.is_closed());
    }
}
