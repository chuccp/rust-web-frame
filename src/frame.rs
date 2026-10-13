//! The application entry point: [`Builder`] collects components, [`WebFrame`] runs them.

use std::sync::Arc;

use axum::Router;

use crate::config::Config;
use crate::core::{
    Context, ModelGroupBuilder, RegisteredGroup, Registration, Rest, RestGroup, Runner, Runtime,
    Service, WiredComponents, init_all,
};
use crate::db;
use crate::log::{self, LOG_CONFIG_KEY, LogConfig};
use crate::model::Table;
use crate::web::{Converter, Filter, Handler, Result, RouteHandle, Routes, Server};

/// Assembles an application.
///
/// ```no_run
/// use rust_web_frame as wf;
/// use wf::web::{handler, MetaOption};
///
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// let mut builder = wf::Builder::new(wf::config::load_auto_config()?);
/// builder.get("/", handler(|_| async { Ok("hello world") }));
/// builder.get("/api/profile", handler(|_| async { Ok("profile") }))
///        .with_meta(MetaOption::value("require_auth", true));
///
/// builder.build().run().await?;
/// # Ok(())
/// # }
/// ```
///
/// Components are initialised in registration order, and a component can only look up
/// the ones registered before it — register dependencies first.
pub struct Builder {
    config: Config,
    registrations: Vec<Registration>,
    model_groups: Vec<RegisteredGroup>,
    rest_groups: Vec<RestGroup>,
    routes: Arc<Routes>,
    inline: Vec<InlineRoutes>,
    logging: Option<LogConfig>,
}

type InlineRoutes = Arc<dyn Fn(&Context) + Send + Sync>;

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("components", &self.registrations.len())
            .field("modelGroups", &self.model_groups.len())
            .field("restGroups", &self.rest_groups.len())
            .field("routes", &self.routes.len())
            .finish_non_exhaustive()
    }
}

impl Builder {
    /// Starts an application from `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            registrations: Vec::new(),
            model_groups: Vec::new(),
            rest_groups: Vec::new(),
            routes: Arc::new(Routes::new()),
            inline: Vec::new(),
            logging: None,
        }
    }

    /// Starts an application, loading configuration from the usual locations.
    ///
    /// # Errors
    ///
    /// Fails when a config file exists but cannot be parsed.
    pub fn auto() -> Result<Self> {
        Ok(Self::new(crate::config::load_auto_config()?))
    }

    /// The configuration this application will run with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Replaces the logging configuration, overriding the `log` config section.
    #[must_use]
    pub fn logging(mut self, config: LogConfig) -> Self {
        self.logging = Some(config);
        self
    }

    // ----------------------------------------------------------- components

    /// Registers a service. It will be initialised at startup and can be looked up with
    /// [`Context::service`].
    pub fn service<S: Service>(&mut self, service: S) -> &mut Self {
        self.registrations.push(Registration::service(service));
        self
    }

    /// Registers a controller. It joins the default server.
    pub fn rest<R: Rest>(&mut self, rest: R) -> &mut Self {
        self.registrations.push(Registration::rest(rest));
        self
    }

    /// Registers a filter that runs for every request on the default server.
    pub fn filter<F: Filter + Send + Sync + 'static>(&mut self, filter: F) -> &mut Self {
        self.registrations.push(Registration::filter(filter));
        self
    }

    /// Registers a background task.
    pub fn runner<R: Runner>(&mut self, runner: R) -> &mut Self {
        self.registrations.push(Registration::runner(runner));
        self
    }

    /// Replaces the default server's response converter.
    pub fn converter<C: Converter + Service>(&mut self, converter: C) -> &mut Self {
        self.registrations.push(Registration::converter(converter));
        self
    }

    /// Registers a model in the default model group. Its table is created at startup.
    pub fn model<M: Table>(&mut self, model: M) -> &mut Self {
        let existing = self
            .model_groups
            .iter_mut()
            .find(|group| group.group.name() == crate::core::DEFAULT_GROUP);
        match existing {
            Some(group) => group.add_model(model),
            None => {
                let mut builder = ModelGroupBuilder::new(crate::core::DEFAULT_GROUP);
                builder = builder.model(model);
                self.model_groups.push(RegisteredGroup::new(builder));
            }
        }
        self
    }

    /// Registers several models in the default model group.
    pub fn models<I, M>(&mut self, models: I) -> &mut Self
    where
        I: IntoIterator<Item = M>,
        M: Table,
    {
        for model in models {
            self.model(model);
        }
        self
    }

    /// Adds a model group bound to its own database.
    pub fn model_group(&mut self, group: ModelGroupBuilder) -> &mut Self {
        self.model_groups.push(RegisteredGroup::new(group));
        self
    }

    /// Adds a group of controllers served on their own port.
    pub fn rest_group(&mut self, group: RestGroup) -> &mut Self {
        self.rest_groups.push(group);
        self
    }

    /// Registers routes on the default server with a closure.
    ///
    /// Useful when a set of routes needs metadata or shared state and a whole controller
    /// type would be overkill:
    ///
    /// ```
    /// # use rust_web_frame::Builder;
    /// # use rust_web_frame::config::Config;
    /// # use rust_web_frame::web::{handler, MetaOption};
    /// let mut builder = Builder::new(Config::empty());
    /// builder.routes(|ctx| {
    ///     ctx.get("/metrics", handler(|_| async { Ok("42") }))
    ///        .with_meta(MetaOption::key(["public"]));
    /// });
    /// ```
    pub fn routes<F>(&mut self, register: F) -> &mut Self
    where
        F: Fn(&Context) + Send + Sync + 'static,
    {
        self.inline.push(Arc::new(register));
        self
    }

    // ---------------------------------------------------------------- routes

    /// Registers a `GET` route on the default server.
    pub fn get(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.get(path, endpoint)
    }

    /// Registers a `POST` route on the default server.
    pub fn post(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.post(path, endpoint)
    }

    /// Registers a `PUT` route on the default server.
    pub fn put(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.put(path, endpoint)
    }

    /// Registers a `DELETE` route on the default server.
    pub fn delete(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.delete(path, endpoint)
    }

    /// Registers a `PATCH` route on the default server.
    pub fn patch(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.patch(path, endpoint)
    }

    /// Registers a route answering every HTTP method.
    pub fn any(&mut self, path: &str, endpoint: Handler) -> RouteHandle {
        self.routes.any(path, endpoint)
    }

    // ----------------------------------------------------------- finishing

    /// Finishes the builder, producing an application that can be run or tested.
    #[must_use]
    pub fn build(self) -> WebFrame {
        WebFrame {
            config: self.config,
            registrations: self.registrations,
            model_groups: self.model_groups,
            rest_groups: self.rest_groups,
            routes: self.routes,
            inline: self.inline,
            logging: self.logging,
        }
    }
}

/// A fully described application.
///
/// Initialisation happens in [`run`](Self::run), [`build_runtime`](Self::build_runtime)
/// or [`init`](Self::init) — never in the builder — so a [`WebFrame`] is an inert
/// description until one of those is called.
pub struct WebFrame {
    config: Config,
    registrations: Vec<Registration>,
    model_groups: Vec<RegisteredGroup>,
    rest_groups: Vec<RestGroup>,
    routes: Arc<Routes>,
    inline: Vec<InlineRoutes>,
    logging: Option<LogConfig>,
}

impl std::fmt::Debug for WebFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebFrame")
            .field("components", &self.registrations.len())
            .field("modelGroups", &self.model_groups.len())
            .field("restGroups", &self.rest_groups.len())
            .field("routes", &self.routes.len())
            .finish_non_exhaustive()
    }
}

impl WebFrame {
    /// The configuration this application was built with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Initialises every component and binds nothing.
    ///
    /// Use it to drive an application in a test, or to embed it in a server you already
    /// run.
    ///
    /// ```no_run
    /// # use rust_web_frame as wf;
    /// # async fn demo(app: wf::WebFrame) -> Result<(), Box<dyn std::error::Error>> {
    /// app.init(|ctx| async move {
    ///     let _ = ctx.service::<String>();
    ///     Ok(())
    /// })
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn init<T, F, Fut>(self, inspect: F) -> Result<T>
    where
        F: FnOnce(Context) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let started = self.start().await?;
        inspect(started.context).await
    }

    /// Initialises the application and returns an axum [`Router`] for the default port.
    ///
    /// The router is exactly what [`run`](Self::run) serves, so an integration test can
    /// drive it with `tower::ServiceExt::oneshot`.
    pub async fn into_router(self) -> Result<Router> {
        let started = self.start().await?;
        let server = started
            .servers
            .first()
            .ok_or_else(|| crate::web::Error::internal().detail("no server was configured"))?
            .clone();
        Ok(server.router(started.context))
    }

    /// Initialises the application and runs it until the process is asked to stop.
    ///
    /// Listens for `Ctrl+C` (and `SIGTERM` on Unix) and then drains in-flight requests.
    pub async fn run(self) -> Result<()> {
        self.run_until(shutdown_signal()).await
    }

    /// Initialises the application and runs it until `signal` resolves.
    pub async fn run_until<F>(self, signal: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let runtime = self.build_runtime().await?;
        runtime.run_until(signal).await
    }

    /// Initialises the application and returns its [`Runtime`], ready to be served.
    pub async fn build_runtime(self) -> Result<Runtime> {
        let started = self.start().await?;
        Ok(Runtime::new(
            started.context,
            started.servers,
            started.runners,
        ))
    }

    async fn start(mut self) -> Result<Started> {
        // The guard belongs to the process; nothing here holds it.
        log::init_logger(resolve_log_config(&self.config, self.logging));

        let default_config = crate::core::default_server_config(&self.config);

        // The default server exists before the context, so controllers can register
        // routes on it while they initialise.
        let default_server = Arc::new(Server::new(default_config.clone()));
        default_server.merge_routes(&self.routes);

        let context = Context::new(self.config.clone(), Arc::clone(&default_server));

        let database = db::connect_from_config(&self.config).await?;
        if let Some(database) = &database {
            context.set_database(database.clone());
        }

        for group in &mut self.model_groups {
            if group.group.db().is_none()
                && let Some(database) = &database
            {
                // A group that was not given its own connection borrows the
                // application's.
                group.group.set_db(database.clone());
            }
            group.group.init(&context, group.auto_create_table).await?;
        }

        // Override from the config *before* checking the ports, or a config that moves
        // two groups onto the same port goes unnoticed.
        let mut rest_groups = self.rest_groups;
        for group in &mut rest_groups {
            group.apply_config(&self.config);
        }
        crate::core::ensure_distinct_ports(&rest_groups, default_config.port)?;

        let mut servers = vec![Arc::clone(&default_server)];
        let mut rest_group_contexts = Vec::with_capacity(rest_groups.len());
        for group in rest_groups {
            let server = Arc::new(Server::new(group.server_config().clone()));
            servers.push(Arc::clone(&server));
            rest_group_contexts.push((group, Arc::clone(&server), context.with_server(server)));
        }

        let WiredComponents {
            filters,
            converter,
            runners,
        } = init_all(self.registrations, &context).await?;
        wire(&default_server, filters, converter);
        for inline in &self.inline {
            inline(&context);
        }

        let mut all_runners = runners;
        for (group, server, group_context) in rest_group_contexts {
            let name = group.name().to_string();
            let WiredComponents {
                filters,
                converter,
                runners,
            } = init_all(group.into_registrations(), &group_context).await?;
            wire(&server, filters, converter);
            tracing::debug!(group = %name, port = server.port(), "rest group ready");
            all_runners.extend(runners);
        }

        for server in &servers {
            server.validate()?;
        }

        Ok(Started {
            context,
            servers,
            runners: all_runners,
        })
    }
}

struct Started {
    context: Context,
    servers: Vec<Arc<Server>>,
    runners: Vec<Arc<dyn Runner>>,
}

fn wire(
    server: &Arc<Server>,
    filters: Vec<Arc<dyn Filter>>,
    converter: Option<Arc<dyn Converter>>,
) {
    for filter in filters {
        server.add_filter(filter);
    }
    if let Some(converter) = converter {
        server.set_converter(converter);
    }
}

fn resolve_log_config(config: &Config, override_config: Option<LogConfig>) -> LogConfig {
    if let Some(config) = override_config {
        return config;
    }
    config.section(LOG_CONFIG_KEY).unwrap_or_default()
}

/// Resolves when the process receives `Ctrl+C`, or `SIGTERM` on Unix.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "cannot listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
