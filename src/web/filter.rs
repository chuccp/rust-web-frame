//! The handler signature and the filter (middleware) chain.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::Method;

use super::error::{Error, Result};
use super::request::Request;
use super::response::{IntoResponse, Response};

/// A boxed, `'static` future — the return type of every handler and filter.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The framework's endpoint signature: takes the request, returns a response or an error.
///
/// Build these with [`handler`]; you never construct one by hand.
pub type HandlerFunc = Arc<dyn Fn(Request) -> BoxFuture<Result<Response>> + Send + Sync>;

/// Wraps an async function or closure into a [`HandlerFunc`].
///
/// ```no_run
/// # use rust_web_frame::web::{handler, Request, Response, Message};
/// let h = handler(|req: Request| async move {
///     let name = req.query("name").unwrap_or("world").to_string();
///     Ok(Response::text(format!("hello {name}")))
/// });
/// ```
///
/// The closure may return anything implementing [`IntoResponse`] — a [`String`], a
/// [`serde_json::Value`], a [`Message`](super::Message), or a [`Response`]:
///
/// ```no_run
/// # use rust_web_frame::web::{handler, Message};
/// let h = handler(|_req| async { Ok(Message::data(serde_json::json!({"ok": true}))) });
/// ```
///
/// Named `async fn`s work too, which is how REST controllers are written:
///
/// ```no_run
/// # use rust_web_frame::web::{handler, Request, Response};
/// async fn index(_req: Request) -> Result<&'static str, rust_web_frame::web::Error> {
///     Ok("hello world")
/// }
/// let h = handler(index);
/// ```
pub fn handler<F, Fut, T>(f: F) -> HandlerFunc
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T, Error>> + Send + 'static,
    T: IntoResponse,
{
    Arc::new(move |req| {
        let fut = f(req);
        Box::pin(async move { fut.await.map(IntoResponse::into_response) })
    })
}

/// A request filter — the framework's middleware.
///
/// A filter runs for every request on the server it was registered with. It decides for
/// itself whether a given route opted in, by reading the route's metadata with
/// [`Request::has_meta`]:
///
/// ```no_run
/// # use rust_web_frame::web::{Filter, FilterChain, Request, Response, Error, MetaOption, Result};
/// # fn require_auth() -> MetaOption { MetaOption::value("require_auth", true) }
/// struct AuthFilter;
///
/// #[async_trait::async_trait]
/// impl Filter for AuthFilter {
///     async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
///         if !req.has_meta(require_auth()) || req.has_meta(MetaOption::value("skip_auth", true)) {
///             return chain.next(req).await; // route didn't opt in
///         }
///         if req.header("authorization").is_none() {
///             return Err(Error::unauthorized());
///         }
///         chain.next(req).await
///     }
/// }
/// ```
#[async_trait]
pub trait Filter: Send + Sync + 'static {
    /// Called once at startup, before any request is served.
    ///
    /// This is where a filter reads configuration or looks up the components it depends
    /// on, exactly like [`Service::init`](crate::core::Service::init).
    async fn init(&self, _ctx: &crate::core::Context) -> Result<()> {
        Ok(())
    }

    /// A human readable name, used in logs and error messages.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Routes the request, optionally delegating downstream with [`FilterChain::next`].
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response>;
}

/// A filter built from a closure.
///
/// The closure runs before the handler and may reject the request by returning an error;
/// to post-process the response, implement [`Filter`] directly.
///
/// ```
/// # use rust_web_frame::web::{filter_fn, Request, Result, Error};
/// let require_token = filter_fn(|req: Request| async move {
///     if req.header("authorization").is_none() {
///         return Err(Error::unauthorized());
///     }
///     Ok(())
/// });
/// ```
pub struct FnFilter<F> {
    guard: F,
}

impl<F> FnFilter<F> {
    /// Wraps `guard`.
    pub fn new(guard: F) -> Self {
        Self { guard }
    }
}

/// Builds a [`FnFilter`] from a closure that runs before the handler.
pub fn filter_fn<F>(guard: F) -> FnFilter<F> {
    FnFilter::new(guard)
}

#[async_trait]
impl<F, Fut> Filter for FnFilter<F>
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), Error>> + Send + 'static,
{
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        (self.guard)(req.clone()).await?;
        chain.next(req).await
    }
}

/// The remaining filters plus the endpoint, ready to be invoked with [`next`](Self::next).
pub struct FilterChain {
    index: isize,
    filters: Arc<Vec<Arc<dyn Filter>>>,
    handler: HandlerFunc,
}

impl FilterChain {
    pub(crate) fn new(filters: Arc<Vec<Arc<dyn Filter>>>, handler: HandlerFunc) -> Self {
        Self {
            index: -1,
            filters,
            handler,
        }
    }

    /// Runs the next filter, or the endpoint when the chain is exhausted.
    ///
    /// A filter may call this more than once, and may post-process the result:
    ///
    /// ```no_run
    /// # use rust_web_frame::web::{Filter, FilterChain, Request, Response, Result};
    /// # struct Timing;
    /// # #[async_trait::async_trait]
    /// # impl Filter for Timing {
    /// # async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
    /// let response = chain.next(req).await?.set_header("x-served-by", "rust-web-frame");
    /// Ok(response)
    /// # }
    /// # }
    /// ```
    pub async fn next(&mut self, req: Request) -> Result<Response> {
        self.index += 1;
        let index = self.index as usize;
        if index < self.filters.len() {
            let filter = self.filters[index].clone();
            filter.handle(self, req).await
        } else {
            let handler = self.handler.clone();
            handler(req).await
        }
    }

    /// How many filters remain downstream, including the endpoint.
    pub fn remaining(&self) -> usize {
        self.filters
            .len()
            .saturating_sub(self.index.max(0) as usize)
    }
}

/// Every HTTP method the framework is willing to route.
pub fn all_methods() -> Vec<Method> {
    vec![
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::HEAD,
        Method::OPTIONS,
        Method::CONNECT,
        Method::TRACE,
    ]
}

/// The methods used by WebSocket and SSE endpoints.
pub fn standard_methods() -> Vec<Method> {
    vec![Method::GET, Method::POST, Method::PUT, Method::DELETE]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{Message, ResponseBody, handler};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counter(Arc<AtomicUsize>);

    #[async_trait]
    impl Filter for Counter {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            self.0.fetch_add(1, Ordering::SeqCst);
            chain.next(req).await
        }
    }

    #[tokio::test]
    async fn chain_runs_filters_in_order_then_the_handler() {
        let seen = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));

        struct Record(&'static str, Arc<std::sync::Mutex<Vec<&'static str>>>);
        #[async_trait]
        impl Filter for Record {
            async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
                self.1.lock().unwrap().push(self.0);
                chain.next(req).await
            }
        }

        let filters = Arc::new(vec![
            Arc::new(Record("first", order.clone())) as Arc<dyn Filter>,
            Arc::new(Record("second", order.clone())),
        ]);
        let mut chain = FilterChain::new(filters, handler(|_| async { Ok(Message::ok()) }));
        let response = chain.next(Request::for_test("/")).await.unwrap();

        assert!(matches!(response.body(), ResponseBody::Message(_)));
        assert_eq!(*order.lock().unwrap(), vec!["first", "second"]);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        let _ = Counter(seen);
    }

    #[tokio::test]
    async fn a_filter_can_short_circuit() {
        struct Deny;
        #[async_trait]
        impl Filter for Deny {
            async fn handle(&self, _chain: &mut FilterChain, _req: Request) -> Result<Response> {
                Err(Error::forbidden())
            }
        }

        let filters = Arc::new(vec![Arc::new(Deny) as Arc<dyn Filter>]);
        let mut chain = FilterChain::new(filters, handler(|_| async { Ok(Message::ok()) }));
        let err = chain.next(Request::for_test("/")).await.unwrap_err();
        assert_eq!(err.code, 403);
    }
}
