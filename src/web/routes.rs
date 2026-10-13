//! The route table.
//!
//! A [`Routes`] is a growable collection of [`Route`]s. [`Context`](crate::core::Context)
//! exposes the same registration methods and forwards them to the server's table, so a
//! controller can register routes from inside its `init`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::Method;

use super::BoxFuture;
use super::error::{Error, Result};
use super::filter::{Handler, all_methods, handler, standard_methods};
use super::request::Request;
use super::response::{Response, ResponseBody};
use super::route::{Route, RouteHandle, RouteKind};
use super::sse::{SseHandler, SseStream};
use super::ws::{WebSocketHandler, WebSocketStream};

/// Collects routes.
///
/// ```
/// # use rust_web_frame::web::{handler, Routes};
/// let routes = Routes::new();
/// routes.get("/", handler(|_| async { Ok("hello world") }));
/// routes.get("/users/:id", handler(|req| async move {
///     Ok(format!("user {}", req.param_or("id", "?")))
/// }));
/// assert_eq!(routes.len(), 2);
/// ```
#[derive(Debug, Default)]
pub struct Routes {
    entries: Mutex<Vec<Arc<Route>>>,
}

impl Routes {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reports whether no routes have been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many routes have been registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().expect("route table poisoned").len()
    }

    /// A snapshot of the registered routes.
    #[must_use]
    pub fn entries(&self) -> Vec<Arc<Route>> {
        self.entries.lock().expect("route table poisoned").clone()
    }

    /// Registers a route for several methods.
    pub fn on(
        &self,
        methods: impl IntoIterator<Item = Method>,
        path: &str,
        endpoint: Handler,
    ) -> RouteHandle {
        self.push(Route::new(path, methods.into_iter().collect(), endpoint))
    }

    /// Registers a route for several methods, tagged with a [`RouteKind`].
    pub(crate) fn push_kind(
        &self,
        methods: Vec<Method>,
        path: &str,
        endpoint: Handler,
        kind: RouteKind,
    ) -> RouteHandle {
        self.push(Route::new(path, methods, endpoint).of_kind(kind))
    }

    /// Appends routes that were registered elsewhere, sharing the same handles so later
    /// metadata still lands on the served route.
    pub(crate) fn extend(&self, routes: impl IntoIterator<Item = Arc<Route>>) {
        self.entries
            .lock()
            .expect("route table poisoned")
            .extend(routes);
    }

    fn push(&self, route: Route) -> RouteHandle {
        let shared = Arc::new(route);
        self.entries
            .lock()
            .expect("route table poisoned")
            .push(shared.clone());
        RouteHandle::new(shared)
    }

    /// Registers a `GET` route.
    pub fn get(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on([Method::GET], path, endpoint)
    }

    /// Registers a `POST` route.
    pub fn post(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on([Method::POST], path, endpoint)
    }

    /// Registers a `PUT` route.
    pub fn put(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on([Method::PUT], path, endpoint)
    }

    /// Registers a `DELETE` route.
    pub fn delete(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on([Method::DELETE], path, endpoint)
    }

    /// Registers a `PATCH` route.
    pub fn patch(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on([Method::PATCH], path, endpoint)
    }

    /// Registers a route answering every HTTP method.
    pub fn any(&self, path: &str, endpoint: Handler) -> RouteHandle {
        self.on(all_methods(), path, endpoint)
    }

    /// Registers a Server-Sent Events endpoint.
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{Routes, SseStream, Result};
    /// # let routes = Routes::new();
    /// routes.sse("/events", |stream: SseStream| Box::pin(async move {
    ///     stream.send("greeting", "hello");
    ///     Ok(())
    /// }));
    /// ```
    pub fn sse<F>(&self, path: &str, endpoint: F) -> RouteHandle
    where
        F: Fn(SseStream) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        let endpoint: SseHandler = Arc::new(endpoint);
        let handler = handler(move |_req: Request| {
            let endpoint = endpoint.clone();
            async move { Ok(Response::new(ResponseBody::Sse(endpoint))) }
        });
        self.push_kind(standard_methods(), path, handler, RouteKind::Sse)
    }

    /// Registers a WebSocket endpoint.
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{Routes, WebSocketStream, Result};
    /// # let routes = Routes::new();
    /// routes.websocket("/ws", |mut socket: WebSocketStream| Box::pin(async move {
    ///     while let Some(frame) = socket.read().await {
    ///         if frame.is_text() {
    ///             socket.write_text(frame.text()).await?;
    ///         }
    ///     }
    ///     Ok(())
    /// }));
    /// ```
    pub fn websocket<F>(&self, path: &str, endpoint: F) -> RouteHandle
    where
        F: Fn(WebSocketStream) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        let endpoint: WebSocketHandler = Arc::new(endpoint);
        let handler = handler(move |_req: Request| {
            let endpoint = endpoint.clone();
            async move { Ok(Response::new(ResponseBody::WebSocket(endpoint))) }
        });
        self.push_kind(standard_methods(), path, handler, RouteKind::WebSocket)
    }

    /// Serves `directory` under `path`, e.g. `static_files("/assets", "./public")`.
    ///
    /// Requests may not escape `directory`: `..` segments and symlinks pointing outside
    /// the root are rejected with `403`.
    pub fn static_files(&self, path: &str, directory: impl Into<PathBuf>) -> RouteHandle {
        let directory = Arc::new(directory.into());
        let pattern = format!("{}/*filepath", path.trim_end_matches('/'));
        let handler = handler(move |req: Request| {
            let directory = Arc::clone(&directory);
            async move { serve_file(&directory, req.param_or("filepath", "")).await }
        });
        self.push_kind(
            vec![Method::GET, Method::HEAD],
            &pattern,
            handler,
            RouteKind::Static,
        )
    }
}

/// Reads `relative` from `directory`, refusing to escape the root.
pub(crate) async fn serve_file(directory: &std::path::Path, relative: &str) -> Result<Response> {
    let relative = relative.trim_start_matches('/');
    let root = tokio::fs::canonicalize(directory)
        .await
        .map_err(|_| Error::not_found().detail("static directory does not exist"))?;
    let resolved = tokio::fs::canonicalize(directory.join(relative))
        .await
        .map_err(|_| Error::not_found().detail(format!("{relative} not found")))?;

    if !resolved.starts_with(&root) {
        return Err(Error::forbidden().detail("path escapes the static root"));
    }

    match tokio::fs::read(&resolved).await {
        Ok(bytes) => {
            let response = axum::response::Response::builder()
                .status(200)
                .header("content-type", super::converter::content_type_for(relative))
                .body(axum::body::Body::from(bytes))
                .map_err(Error::from)?;
            Ok(Response::raw(response))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::not_found().detail(format!("{relative} not found")))
        }
        Err(err) => Err(Error::from(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{Message, MetaOption};

    #[test]
    fn routes_are_collected_and_handed_out() {
        let routes = Routes::new();
        assert!(routes.is_empty());

        let handle = routes.get("/users/:id", handler(|_| async { Ok(Message::ok()) }));
        assert_eq!(handle.path(), "/users/:id");
        assert_eq!(handle.methods(), &[Method::GET]);
        assert!(handle.accepts(&Method::GET));
        assert!(!handle.accepts(&Method::POST));
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn metadata_attached_through_a_handle_reaches_the_stored_route() {
        let routes = Routes::new();
        routes
            .post("/admin", handler(|_| async { Ok("ok") }))
            .with_meta(MetaOption::value("require_auth", true))
            .with_meta(MetaOption::key(["admin"]));

        let stored = &routes.entries()[0];
        assert!(stored.meta().contains_entry("require_auth", &true.into()));
        assert!(stored.meta().contains_key("admin"));
    }

    #[test]
    fn static_mounts_get_a_wildcard_and_two_methods() {
        let routes = Routes::new();
        let handle = routes.static_files("/assets", "./public");
        assert_eq!(handle.path(), "/assets/*filepath");
        assert_eq!(handle.methods(), &[Method::GET, Method::HEAD]);
        assert_eq!(handle.kind(), RouteKind::Static);
    }

    #[tokio::test]
    async fn static_serving_refuses_to_escape_its_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "hi").unwrap();
        let outside = dir.path().parent().unwrap().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();

        let response = serve_file(dir.path(), "ok.txt").await.unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let err = serve_file(dir.path(), "../secret.txt").await.unwrap_err();
        assert_eq!(err.code, 403);

        let _ = std::fs::remove_file(outside);
    }
}
