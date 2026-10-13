//! Serving the application and supervising its background tasks.

use std::sync::Arc;

use std::time::Duration;

use tokio::task::JoinSet;

use super::context::Context;
use super::service::Runner;
use crate::web::{Result, Server};

/// How long shutdown keeps waiting for background tasks after the context is cancelled.
///
/// A runner that does not watch
/// [`Context::cancellation_token`] never returns, and waiting for it forever would make
/// the process impossible to stop. Past this bound the remaining tasks are abandoned —
/// dropping the [`JoinSet`] aborts them — and a warning says how many were left.
///
/// Requests are not affected: listeners stop accepting and drain within
/// `axum`'s own graceful shutdown, and that happens before this bound applies.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The bound servers and background tasks of a running application.
///
/// Built by [`WebFrame`](crate::WebFrame) once every component has been initialised;
/// [`run`](Self::run) is what actually blocks.
pub struct Runtime {
    context: Context,
    servers: Vec<Arc<Server>>,
    runners: Vec<Arc<dyn Runner>>,
    shutdown_grace: Duration,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("ports", &self.ports())
            .field("runners", &self.runners.len())
            .field("shutdownGrace", &self.shutdown_grace)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Collects the pieces of a runnable application.
    pub(crate) fn new(
        context: Context,
        servers: Vec<Arc<Server>>,
        runners: Vec<Arc<dyn Runner>>,
    ) -> Self {
        Self {
            context,
            servers,
            runners,
            shutdown_grace: SHUTDOWN_GRACE,
        }
    }

    /// Overrides how long shutdown waits for background tasks.
    ///
    /// Only the tests use this: they cannot afford to sit through the real bound.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// The application context.
    #[must_use]
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The ports this application listens on.
    #[must_use]
    pub fn ports(&self) -> Vec<u16> {
        self.servers.iter().map(|server| server.port()).collect()
    }

    /// Serves every port until the context is cancelled, or until a server fails.
    ///
    /// Background runners are spawned first. On shutdown they are given
    /// [`SHUTDOWN_GRACE`] to notice the cancellation and return; anything still running
    /// after that is abandoned with a warning rather than blocking the process forever.
    pub async fn run(self) -> Result<()> {
        let mut tasks: JoinSet<Result<()>> = JoinSet::new();

        for runner in &self.runners {
            let runner = Arc::clone(runner);
            let context = self.context.clone();
            let name = runner.name();
            tracing::info!(runner = name, "runner started");
            tasks.spawn(async move {
                let outcome = runner.run(&context).await;
                if let Err(error) = &outcome {
                    tracing::error!(runner = name, error = %error, "runner stopped");
                }
                outcome
            });
        }

        for server in &self.servers {
            let server = Arc::clone(server);
            let context = self.context.clone();
            tasks.spawn(async move { server.serve(context).await });
        }

        let mut failures = Vec::new();
        let mut deadline: Option<tokio::time::Instant> = None;

        while !tasks.is_empty() {
            // Start the clock the moment shutdown is requested, not per task.
            if self.context.is_shutting_down() && deadline.is_none() {
                deadline = Some(tokio::time::Instant::now() + self.shutdown_grace);
            }

            let joined = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                        Ok(joined) => joined,
                        Err(_) => {
                            tracing::warn!(
                                grace = ?self.shutdown_grace,
                                abandoned = tasks.len(),
                                "shutdown grace expired, abandoning the tasks that did not stop"
                            );
                            // Dropping the set aborts whatever is left.
                            break;
                        }
                    }
                }
                None => tasks.join_next().await,
            };

            match joined {
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(error))) => {
                    tracing::error!(error = %error, "server stopped with an error");
                    failures.push(error);
                    // A bound port failing is fatal for the whole application.
                    self.context.shutdown();
                }
                Some(Err(join_error)) => {
                    tracing::error!(error = %join_error, "task panicked");
                    self.context.shutdown();
                }
                None => break,
            }
        }

        match failures.into_iter().next() {
            Some(error) if !self.context.is_shutting_down() => Err(error),
            Some(_) => Ok(()),
            None => Ok(()),
        }
    }

    /// Runs the application until `signal` resolves, then drains in-flight requests.
    ///
    /// ```no_run
    /// # use rust_web_frame as wf;
    /// # async fn demo(app: wf::WebFrame) -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = app.build_runtime().await?;
    /// runtime
    ///     .run_until(async {
    ///         tokio::signal::ctrl_c().await.ok();
    ///     })
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run_until<F>(self, signal: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let context = self.context.clone();
        let trigger = context.clone();
        tokio::spawn(async move {
            signal.await;
            tracing::info!("shutdown signal received, draining");
            trigger.shutdown();
        });
        self.run().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ServerConfig;
    use async_trait::async_trait;

    /// A runner that never looks at the cancellation token.
    struct Stubborn;

    #[async_trait]
    impl Runner for Stubborn {
        async fn run(&self, _ctx: &Context) -> Result<()> {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }

        fn name(&self) -> &'static str {
            "Stubborn"
        }
    }

    /// A runner that does the right thing.
    #[derive(Default)]
    struct Polite(std::sync::Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl Runner for Polite {
        async fn run(&self, ctx: &Context) -> Result<()> {
            ctx.cancellation_token().cancelled().await;
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn name(&self) -> &'static str {
            "Polite"
        }
    }

    /// Binds port 0, so the tests never collide with a real service.
    fn idle_server() -> Arc<Server> {
        Arc::new(Server::new(ServerConfig::on_port(0)))
    }

    #[tokio::test]
    async fn a_runner_that_ignores_cancellation_cannot_block_shutdown() {
        let context = Context::empty();
        let trigger = context.clone();
        let grace = Duration::from_millis(200);

        let runtime = Runtime::new(
            context,
            vec![idle_server()],
            vec![Arc::new(Stubborn) as Arc<dyn Runner>],
        )
        .with_shutdown_grace(grace);

        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            runtime.run_until(async move { trigger.shutdown() }),
        )
        .await;

        assert!(outcome.is_ok(), "run() never returned");
        assert!(outcome.unwrap().is_ok(), "shutdown is not a failure");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown waited far past the grace: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_polite_runner_is_allowed_to_finish() {
        let context = Context::empty();
        let trigger = context.clone();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let runtime = Runtime::new(
            context,
            vec![idle_server()],
            vec![Arc::new(Polite(stopped.clone())) as Arc<dyn Runner>],
        )
        .with_shutdown_grace(Duration::from_secs(5));

        runtime
            .run_until(async move { trigger.shutdown() })
            .await
            .unwrap();

        assert!(
            stopped.load(std::sync::atomic::Ordering::SeqCst),
            "the runner should have been given time to stop cleanly"
        );
    }

    #[test]
    fn ports_are_reported_in_registration_order() {
        let context = Context::empty();
        let runtime = Runtime::new(
            context,
            vec![
                Arc::new(Server::new(crate::web::ServerConfig::on_port(8081))),
                Arc::new(Server::new(crate::web::ServerConfig::on_port(8082))),
            ],
            Vec::new(),
        );
        assert_eq!(runtime.ports(), vec![8081, 8082]);
    }
}
