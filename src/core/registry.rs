//! What the application remembers about a component between registration and startup.

use std::any::{Any, TypeId};
use std::sync::Arc;

use super::context::Context;
use super::service::{
    InitFn, Rest, Runner, Service, WireFn, Wired, filter_init, init_failed, rest_init, runner_init,
    service_init, wire_converter, wire_filter, wire_runner, wire_service,
};
use crate::web::{Converter, Error, Filter, Result};

/// One component, as registered but not yet initialised.
///
/// The component lives behind an `Arc` from registration onwards, so the same instance
/// is used for its `init`, for its role handles (a filter, a runner) and for lookups
/// through [`Context::service`].
pub(crate) struct Registration {
    name: &'static str,
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
    init: InitFn,
    wire: WireFn,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Registration {
    /// Registers a plain service.
    pub(crate) fn service<S: Service>(service: S) -> Self {
        let value = Arc::new(service);
        Self {
            name: Service::name(&*value),
            type_id: TypeId::of::<S>(),
            init: service_init(Arc::clone(&value)),
            wire: wire_service::<S>(),
            value,
        }
    }

    /// Registers a controller.
    pub(crate) fn rest<R: Rest>(rest: R) -> Self {
        let value = Arc::new(rest);
        Self {
            name: Rest::name(&*value),
            type_id: TypeId::of::<R>(),
            init: rest_init(Arc::clone(&value)),
            wire: wire_service::<R>(),
            value,
        }
    }

    /// Registers a filter. It is both a filter and a service.
    pub(crate) fn filter<F: Filter + Send + Sync + 'static>(filter: F) -> Self {
        let value = Arc::new(filter);
        Self {
            name: Filter::name(&*value),
            type_id: TypeId::of::<F>(),
            init: filter_init(Arc::clone(&value)),
            wire: wire_filter::<F>(),
            value,
        }
    }

    /// Registers a service that is also a response converter.
    pub(crate) fn converter<S: Converter + Service>(converter: S) -> Self {
        let value = Arc::new(converter);
        Self {
            name: Service::name(&*value),
            type_id: TypeId::of::<S>(),
            init: service_init(Arc::clone(&value)),
            wire: wire_converter::<S>(),
            value,
        }
    }

    /// Registers a background runner.
    pub(crate) fn runner<R: Runner>(runner: R) -> Self {
        let value = Arc::new(runner);
        Self {
            name: Runner::name(&*value),
            type_id: TypeId::of::<R>(),
            init: runner_init(Arc::clone(&value)),
            wire: wire_runner::<R>(),
            value,
        }
    }

    /// The Rust type name, for logs and error messages.
    #[must_use]
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    /// The concrete type this component registers under.
    #[must_use]
    pub(crate) fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Initialises the component and returns its role handles.
    ///
    /// The component is not visible to other components until this returns, so a
    /// component can only look up the ones registered before it.
    pub(crate) async fn init(self, ctx: &Context) -> Result<(Arc<dyn Any + Send + Sync>, Wired)> {
        (self.init)(ctx.clone())
            .await
            .map_err(|error| init_failed(self.name, error))?;
        let wired = (self.wire)(&self.value);
        Ok((self.value, wired))
    }
}

/// Initialises every component in order, wiring each one into the context as it becomes
/// ready.
///
/// Returns the filters, converter and runners the caller still has to attach.
pub(crate) struct WiredComponents {
    pub filters: Vec<Arc<dyn Filter>>,
    pub converter: Option<Arc<dyn Converter>>,
    pub runners: Vec<Arc<dyn Runner>>,
}

pub(crate) async fn init_all(
    registrations: Vec<Registration>,
    ctx: &Context,
) -> Result<WiredComponents> {
    let mut wired = WiredComponents {
        filters: Vec::new(),
        converter: None,
        runners: Vec::new(),
    };

    for registration in registrations {
        let name = registration.name();
        let type_id = registration.type_id();
        let (value, roles) = registration.init(ctx).await?;

        // Register under the type the caller wrote, so `ctx.service::<T>()` finds it.
        //
        // The container keys by type, so a second registration of the same type would
        // replace the first — leaving one instance in the container and a *different*
        // one wired into the filter chain. Anything read back would then be a copy that
        // never runs. Refuse instead of picking one silently.
        if let Some(previous) = ctx.registered_name(type_id) {
            return Err(Error::new(500, format!("{name} is registered twice")).detail(format!(
                "{name} is registered twice: it was already registered as {previous}. A component is registered once — a filter, runner or controller is already a service, so registering it with `filter(...)` is enough and a second `service(...)` call would only create a copy that nothing ever calls."
            )));
        }
        ctx.register_dyn(type_id, name, value);
        if let Some(filter) = roles.filter {
            wired.filters.push(filter);
        }
        if let Some(converter) = roles.converter {
            wired.converter = Some(converter);
        }
        if let Some(runner) = roles.runner {
            wired.runners.push(runner);
        }
    }

    Ok(wired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::Error;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counted(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for Counted {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct First(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for First {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Second(Arc<AtomicUsize>);

    #[async_trait]
    impl Service for Second {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Broken;

    #[async_trait]
    impl Service for Broken {
        async fn init(&self, _ctx: &Context) -> Result<()> {
            Err(Error::bad_request().detail("nope"))
        }
    }

    #[tokio::test]
    async fn components_are_initialised_in_order() {
        let ctx = Context::empty();
        let seen = Arc::new(AtomicUsize::new(0));

        let wired = init_all(
            vec![
                Registration::service(First(seen.clone())),
                Registration::service(Second(seen.clone())),
            ],
            &ctx,
        )
        .await
        .unwrap();

        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert!(wired.filters.is_empty());
        assert!(wired.runners.is_empty());
        assert_eq!(ctx.registered_names().len(), 2);
    }

    #[tokio::test]
    async fn registering_the_same_type_twice_is_refused() {
        // Regression: the container keys by type, so a second registration used to
        // replace the first — leaving one instance in the container and a *different*
        // one wired into the filter chain. Anything read back was a copy that never ran.
        let ctx = Context::empty();
        let error = match init_all(
            vec![
                Registration::service(Counted(Arc::new(AtomicUsize::new(0)))),
                Registration::service(Counted(Arc::new(AtomicUsize::new(0)))),
            ],
            &ctx,
        )
        .await
        {
            Ok(_) => panic!("a duplicate registration must be refused"),
            Err(error) => error,
        };

        // `message` is the summary; `detail` (what reaches a client) carries the guidance.
        assert!(
            error.message.contains("registered twice"),
            "{}",
            error.message
        );
        let detail = error.client_message();
        assert!(detail.contains("registered twice"), "{detail}");
        assert!(
            detail.contains("filter"),
            "the message should point out that a filter is already a service: {detail}"
        );
        assert_eq!(
            ctx.registered_names().len(),
            1,
            "the first registration stays; nothing is replaced"
        );
    }

    #[tokio::test]
    async fn a_failing_init_aborts_startup_and_names_the_component() {
        let ctx = Context::empty();
        let err = match init_all(vec![Registration::service(Broken)], &ctx).await {
            Ok(_) => panic!("a failing component must abort startup"),
            Err(err) => err,
        };
        assert!(err.client_message().contains("Broken"));
        assert!(err.client_message().contains("nope"));
    }
}
