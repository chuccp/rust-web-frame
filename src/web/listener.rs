//! The accept loop.
//!
//! `axum::serve` builds its hyper connection with `Builder::new(TokioExecutor::new())` and
//! offers no way in, which would leave `read_header_timeout` and `max_header_bytes` as
//! configuration that changes nothing. The loop is written out here instead, following the
//! shape of axum's own: a task per connection,
//! `serve_connection_with_upgrades` so WebSockets still upgrade, and `ConnectInfo` added
//! from the peer address so [`Request::client_ip`](super::Request::client_ip) works.

use std::net::SocketAddr;
use std::pin::pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as ConnectionBuilder;
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::{Service, ServiceExt as _};

use super::config::ServerConfig;
use super::error::{Error, Result};

/// How long a listener keeps draining in-flight requests after shutdown is requested.
///
/// The same bound the Go original uses. Without it, one handler that never returns would
/// keep the process alive indefinitely.
pub const DRAIN_GRACE: Duration = Duration::from_secs(30);

/// Binds `config.port` and serves `router` until `cancel` is triggered.
///
/// Returns once every in-flight request has finished, or once [`DRAIN_GRACE`] has passed
/// since the cancellation — whichever comes first.
pub(crate) async fn serve(
    router: Router,
    config: &ServerConfig,
    cancel: CancellationToken,
) -> Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(addr).await.map_err(|err| {
        Error::with_source(500, err).detail(format!("cannot bind port {}", config.port))
    })?;

    let header_timeout = config.effective_read_header_timeout();
    let max_header_bytes = config.effective_max_header_bytes();
    let mut connections: JoinSet<()> = JoinSet::new();

    tracing::info!(
        port = config.port,
        headerTimeout = ?header_timeout,
        maxHeaderBytes = max_header_bytes,
        "server listening"
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    // A failed accept is one connection's problem, not the server's.
                    Err(err) => {
                        tracing::warn!(error = %err, "accept failed");
                        continue;
                    }
                };

                let service = WithPeer {
                    inner: router.clone(),
                    peer,
                }
                .map_request(|req: hyper::Request<hyper::body::Incoming>| {
                    req.map(axum::body::Body::new)
                });

                let stopping = cancel.clone();
                connections.spawn(async move {
                    serve_connection(stream, service, stopping, header_timeout, max_header_bytes)
                        .await;
                });
            }
        }
    }

    let open = connections.len();
    if open > 0 {
        tracing::debug!(connections = open, "draining in-flight connections");
    }
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(DRAIN_GRACE, drain).await.is_err() {
        tracing::warn!(
            grace = ?DRAIN_GRACE,
            abandoned = connections.len(),
            "drain grace expired, closing the connections that did not finish"
        );
    }

    Ok(())
}

/// Adds the peer address that [`Request::client_ip`](super::Request::client_ip) reads.
///
/// axum's `into_make_service_with_connect_info` resolves through the `Connected` trait,
/// which is ambiguous here because several impls accept a bare `SocketAddr`. Inserting the
/// extension directly says the same thing without the indirection.
#[derive(Clone, Copy)]
struct WithPeer<S> {
    inner: S,
    peer: SocketAddr,
}

impl<S, B> Service<hyper::Request<B>> for WithPeer<S>
where
    S: Service<hyper::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: hyper::Request<B>) -> Self::Future {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(self.peer));
        self.inner.call(request)
    }
}

/// Serves one connection until it closes, or until shutdown is requested — in which case
/// the request in flight is allowed to finish first.
async fn serve_connection<S>(
    stream: TcpStream,
    service: S,
    stopping: CancellationToken,
    header_timeout: Duration,
    max_header_bytes: usize,
) where
    S: Service<hyper::Request<hyper::body::Incoming>, Response = axum::response::Response>
        + Clone
        + Send
        + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send,
{
    let mut builder = ConnectionBuilder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        // A client that dribbles its headers is disconnected instead of holding a
        // connection open indefinitely.
        .header_read_timeout(header_timeout)
        // Headers larger than this make hyper answer 431 rather than grow a buffer.
        .max_buf_size(max_header_bytes);

    let io = TokioIo::new(stream);
    let service = TowerToHyperService::new(service);
    let mut connection = pin!(builder.serve_connection_with_upgrades(io, service));

    tokio::select! {
        result = connection.as_mut() => {
            if let Err(err) = result {
                tracing::debug!(error = %err, "connection ended with an error");
            }
            return;
        }
        _ = stopping.cancelled() => {
            // Finish the request in flight, then close.
            connection.as_mut().graceful_shutdown();
        }
    }

    if let Err(err) = connection.as_mut().await {
        tracing::debug!(error = %err, "connection ended with an error while draining");
    }
}
