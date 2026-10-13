//! Serving the application and supervising its background tasks.

use std::sync::Arc;

use super::service::Runner;
use crate::web::{Result, Server};
use tokio::task::JoinSet;

use super::context::Context;

/// The bound servers and background tasks of a running application.
///
/// Built by [`WebFrame`](crate::WebFrame) once every component has been initialised;
/// [`run`](Self::run) is what actually blocks.
pub struct Runtime {
    context: Context,
    servers: Vec<Arc<Server>>,
    runners: Vec<Arc<dyn Runner>>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("ports", &self.ports())
            .field("runners", &self.runners.len())
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
        }
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
    /// Background runners are spawned first and stopped when the context is cancelled.
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
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "server stopped with an error");
                    failures.push(error);
                    // A bound port failing is fatal for the whole application.
                    self.context.shutdown();
                }
                Err(join_error) => {
                    tracing::error!(error = %join_error, "task panicked");
                    self.context.shutdown();
                }
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
