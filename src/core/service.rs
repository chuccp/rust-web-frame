//! The component traits every part of an application implements.
//!
//! There are four kinds of component, and each declares its own `init` so a single
//! `impl` block is enough — no super-trait juggling:
//!
//! | Trait | Registered with | Also does |
//! |---|---|---|
//! | [`Service`] | [`Builder::service`](crate::Builder::service) | nothing else |
//! | [`Rest`] | [`Builder::rest`](crate::Builder::rest) | registers routes in `init` |
//! | [`Filter`](crate::web::Filter) | [`Builder::filter`](crate::Builder::filter) | wraps every request |
//! | [`Runner`] | [`Builder::runner`](crate::Builder::runner) | runs in the background |

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

use super::context::Context;
use crate::web::{Converter, Error, Filter, Result};

/// The base trait for anything the container manages.
///
/// [`init`](Service::init) runs once at startup, before any request is served. It is where
/// a component wires itself up: reading configuration, looking up other components.
///
/// ```no_run
/// use async_trait::async_trait;
/// use rust_web_frame::core::{Context, Service};
/// use rust_web_frame::web::Result;
///
/// #[derive(Default)]
/// pub struct Greeter {
///     greeting: std::sync::OnceLock<String>,
/// }
///
/// #[async_trait]
/// impl Service for Greeter {
///     async fn init(&self, ctx: &Context) -> Result<()> {
///         let greeting = ctx.config().get_or("app.greeting", String::from("hello"));
///         let _ = self.greeting.set(greeting);
///         Ok(())
///     }
/// }
/// ```
///
/// `init` takes `&self` so the same instance can be handed to several parts of the
/// framework. State that is only known at startup goes in a
/// [`OnceLock`](std::sync::OnceLock).
///
/// Components are initialised in registration order, and a component can only look up the
/// ones registered before it — register dependencies first.
#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// Called once at startup. Returning an error aborts the whole application.
    async fn init(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }

    /// A human readable name, used in logs and error messages.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// A REST controller: a component whose [`init`](Rest::init) registers routes.
///
/// ```no_run
/// use async_trait::async_trait;
/// use rust_web_frame::core::{Context, Rest};
/// use rust_web_frame::web::{Message, MetaOption, Request, Response, Result, handler};
/// use serde_json::json;
///
/// #[derive(Default)]
/// pub struct UserController;
///
/// impl UserController {
///     async fn list(_req: Request) -> Result<Response> {
///         Ok(Response::json(json!({ "users": [] })))
///     }
///
///     async fn show(req: Request) -> Result<Message> {
///         Ok(Message::data(json!({ "id": req.param_u64("id") })))
///     }
/// }
///
/// #[async_trait]
/// impl Rest for UserController {
///     async fn init(&self, ctx: &Context) -> Result<()> {
///         ctx.get("/api/users", handler(Self::list))
///            .with_meta(MetaOption::key(["skip_auth"]));
///         ctx.get("/api/users/:id", handler(Self::show));
///         Ok(())
///     }
/// }
/// ```
///
/// A handler declares its own return type. Anything implementing
/// [`IntoResponse`](crate::web::IntoResponse) works — a
/// [`Response`](crate::web::Response), a [`Message`](crate::web::Message), a [`String`],
/// or a [`serde_json::Value`].
///
/// Controllers added with [`Builder::rest`](crate::Builder::rest) join the default
/// server; those in a [`RestGroup`](super::RestGroup) get their own port.
#[async_trait]
pub trait Rest: Send + Sync + 'static {
    /// Called once at startup. Register routes on `ctx` here.
    async fn init(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }

    /// A human readable name, used in logs and error messages.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// A background task.
///
/// [`run`](Runner::run) is spawned once the servers are bound and keeps running until the
/// application shuts down. The context's
/// [`cancellation_token`](Context::cancellation_token) tells it when to stop.
///
/// ```no_run
/// use async_trait::async_trait;
/// use rust_web_frame::core::{Context, Runner};
/// use rust_web_frame::web::Result;
///
/// #[derive(Default)]
/// pub struct Cleanup;
///
/// #[async_trait]
/// impl Runner for Cleanup {
///     async fn run(&self, ctx: &Context) -> Result<()> {
///         let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
///         let cancel = ctx.cancellation_token();
///         loop {
///             tokio::select! {
///                 _ = ticker.tick() => { /* sweep expired rows */ }
///                 _ = cancel.cancelled() => return Ok(()),
///             }
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait Runner: Send + Sync + 'static {
    /// Called once at startup, before [`run`](Runner::run) is spawned.
    async fn init(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }

    /// A human readable name, used in logs and error messages.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Runs until the application shuts down.
    async fn run(&self, ctx: &Context) -> Result<()>;
}

/// A response converter that is also a component.
///
/// A converter needs both this and [`Converter`](crate::web::Converter): `Service::init`
/// for its startup work, `Converter::convert` for rendering. Each has exactly one method,
/// so there is nothing to override and two small `impl` blocks are enough.
pub trait ConverterService: Service + Converter {}

// ---------------------------------------------------------------------------
// Erasure
//
// The container stores components by `TypeId`, but Rust has no runtime reflection: a
// `Box<dyn Service>` cannot be handed back as a `Filter` or a `Runner`. Each registration
// therefore carries closures that have already captured the concrete type — `init` calls
// the right trait method, `wire` re-materialises the role handles, and an
// `Arc<dyn Any + Send + Sync>` is what the context hands back on lookup.
//
// Because every component trait declares `init(&self)`, the value can live behind an
// `Arc` from the very start — so a component registered as a filter is still found by
// `ctx.service::<TheFilterType>()`.
// ---------------------------------------------------------------------------

/// Runs a component's `init` against an already-erased value.
pub(crate) type InitFn = Box<dyn Fn(Context) -> crate::web::BoxFuture<Result<()>> + Send + Sync>;

/// Re-materialises a component's concrete type and produces its role handles.
pub(crate) type WireFn = Box<dyn Fn(&Arc<dyn Any + Send + Sync>) -> Wired + Send + Sync>;

/// The role handles for one registered component, all sharing the same instance.
pub(crate) struct Wired {
    pub filter: Option<Arc<dyn Filter>>,
    pub converter: Option<Arc<dyn Converter>>,
    pub runner: Option<Arc<dyn Runner>>,
}

impl Wired {
    /// A component that plays no extra role.
    pub(crate) fn none() -> Self {
        Self {
            filter: None,
            converter: None,
            runner: None,
        }
    }
}

fn downcast<T: Send + Sync + 'static>(any: &Arc<dyn Any + Send + Sync>, role: &str) -> Arc<T> {
    any.clone()
        .downcast::<T>()
        .unwrap_or_else(|_| panic!("registered {role} has an unexpected concrete type"))
}

/// Builds the [`InitFn`] for a value whose initialiser lives on `T`.
///
/// `call` receives owned handles so the future it returns is `'static`; both `Arc` and
/// [`Context`] clone cheaply.
pub(crate) fn init_fn<T, F>(value: Arc<T>, call: F) -> InitFn
where
    T: Send + Sync + 'static,
    F: Fn(Arc<T>, Context) -> crate::web::BoxFuture<Result<()>> + Send + Sync + Copy + 'static,
{
    Box::new(move |ctx: Context| call(Arc::clone(&value), ctx))
}

/// Builds the [`InitFn`] for a plain [`Service`].
pub(crate) fn service_init<S: Service>(value: Arc<S>) -> InitFn {
    init_fn(value, |service, ctx| {
        Box::pin(async move { Service::init(service.as_ref(), &ctx).await })
    })
}

/// Builds the [`InitFn`] for a [`Rest`] controller.
pub(crate) fn rest_init<R: Rest>(value: Arc<R>) -> InitFn {
    init_fn(value, |rest, ctx| {
        Box::pin(async move { Rest::init(rest.as_ref(), &ctx).await })
    })
}

/// Builds the [`InitFn`] for a [`Filter`].
pub(crate) fn filter_init<F: Filter + Send + Sync + 'static>(value: Arc<F>) -> InitFn {
    init_fn(value, |filter, ctx| {
        Box::pin(async move { Filter::init(filter.as_ref(), &ctx).await })
    })
}

/// Builds the [`InitFn`] for a [`Runner`].
pub(crate) fn runner_init<R: Runner>(value: Arc<R>) -> InitFn {
    init_fn(value, |runner, ctx| {
        Box::pin(async move { Runner::init(runner.as_ref(), &ctx).await })
    })
}

/// Builds a [`WireFn`] for a component that only plays the service role.
pub(crate) fn wire_service<T: Send + Sync + 'static>() -> WireFn {
    Box::new(|any| {
        let _ = downcast::<T>(any, "service");
        Wired::none()
    })
}

/// Builds a [`WireFn`] for a component that is also a filter.
pub(crate) fn wire_filter<T: Filter + Send + Sync + 'static>() -> WireFn {
    Box::new(|any| Wired {
        filter: Some(downcast::<T>(any, "filter") as Arc<dyn Filter>),
        converter: None,
        runner: None,
    })
}

/// Builds a [`WireFn`] for a component that is also a converter.
pub(crate) fn wire_converter<T: Converter + Send + Sync + 'static>() -> WireFn {
    Box::new(|any| Wired {
        filter: None,
        converter: Some(downcast::<T>(any, "converter") as Arc<dyn Converter>),
        runner: None,
    })
}

/// Builds a [`WireFn`] for a component that is also a background runner.
pub(crate) fn wire_runner<T: Runner + Send + Sync + 'static>() -> WireFn {
    Box::new(|any| Wired {
        filter: None,
        converter: None,
        runner: Some(downcast::<T>(any, "runner") as Arc<dyn Runner>),
    })
}

/// Turns an [`Error`] into a startup failure that names the component.
pub(crate) fn init_failed(name: &str, error: Error) -> Error {
    let message = format!("{name} failed to initialise: {}", error.client_message());
    error.detail(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{FilterChain, Request, Response};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counter(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for Counter {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A controller that is *also* a filter — the shape the framework has to keep
    /// straight.
    #[derive(Default)]
    struct Controller(Arc<AtomicUsize>);

    #[async_trait]
    impl Rest for Controller {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl Filter for Controller {
        async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
            chain.next(req).await
        }
    }

    #[tokio::test]
    async fn init_fns_call_the_trait_the_component_was_registered_under() {
        let counter = Arc::new(Counter::default());
        let seen = Arc::clone(&counter.0);
        service_init(Arc::clone(&counter))(Context::empty())
            .await
            .unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        let controller = Arc::new(Controller::default());
        let controller_seen = Arc::clone(&controller.0);
        rest_init(Arc::clone(&controller))(Context::empty())
            .await
            .unwrap();
        assert_eq!(controller_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wire_fns_produce_exactly_the_roles_that_were_asked_for() {
        let any: Arc<dyn Any + Send + Sync> = Arc::new(Controller::default());

        let wired = wire_filter::<Controller>()(&any);
        assert!(wired.filter.is_some());
        assert!(wired.converter.is_none());
        assert!(wired.runner.is_none());

        let wired = wire_service::<Controller>()(&any);
        assert!(wired.filter.is_none());
        assert!(wired.converter.is_none());
        assert!(wired.runner.is_none());
    }

    #[tokio::test]
    async fn a_component_keeps_its_own_identity_while_playing_a_role() {
        let any: Arc<dyn Any + Send + Sync> = Arc::new(Controller::default());

        let filter = wire_filter::<Controller>()(&any).filter.unwrap();
        let mut chain = FilterChain::new(
            Arc::new(Vec::new()),
            crate::web::handler(|_| async { Ok("reached") }),
        );
        let response = filter
            .handle(&mut chain, Request::for_test("/"))
            .await
            .unwrap();
        assert!(matches!(response.body(), crate::web::ResponseBody::Text(_)));

        // ...and the context can still find it by its own type.
        assert!(any.downcast_ref::<Controller>().is_some());
    }

    #[test]
    fn wiring_the_wrong_role_is_a_programming_error() {
        let any: Arc<dyn Any + Send + Sync> = Arc::new(Counter::default());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wire_filter::<Controller>()(&any);
        }))
        .is_err();
        assert!(
            panicked,
            "asking for a filter role on a non-filter must panic"
        );
    }
}
