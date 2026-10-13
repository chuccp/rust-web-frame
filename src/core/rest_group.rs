//! Serving a set of controllers on their own port.

use async_trait::async_trait;

use super::context::Context;
use super::registry::Registration;
use super::service::{Rest, Service};
use crate::web::{Converter, Error, Filter, Result, SERVER_CONFIG_KEY, ServerConfig};

/// A set of controllers, filters and a converter bound to their own [`ServerConfig`].
///
/// Use one when part of the application belongs on a different port or behind a
/// different context path — an admin API, a metrics endpoint, a webhook receiver:
///
/// ```no_run
/// use rust_web_frame::core::RestGroup;
/// use rust_web_frame::web::{handler, ServerConfig};
///
/// let admin = RestGroup::builder("admin")
///     .server_config(ServerConfig::on_port(8081).with_context_path("/admin"))
///     .routes(|ctx| {
///         ctx.get("/stats", handler(|_| async { Ok("ok") }));
///     })
///     .build();
/// ```
pub struct RestGroup {
    name: String,
    server_config: ServerConfig,
    registrations: Vec<Registration>,
}

impl std::fmt::Debug for RestGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestGroup")
            .field("name", &self.name)
            .field("port", &self.server_config.port)
            .field("components", &self.registrations.len())
            .finish()
    }
}

impl RestGroup {
    /// Starts building a group called `name`.
    pub fn builder(name: impl Into<String>) -> RestGroupBuilder {
        RestGroupBuilder::new(name)
    }

    /// The group's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The port the group listens on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.server_config.port
    }

    /// The group's server configuration.
    #[must_use]
    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }

    /// The components in this group.
    pub(crate) fn into_registrations(self) -> Vec<Registration> {
        self.registrations
    }

    /// Reads a group from the `web.restGroups.<name>` section of the config, so a port
    /// can be changed without a rebuild.
    pub(crate) fn apply_config(&mut self, config: &crate::config::Config) {
        let key = format!("web.restGroups.{}", self.name);
        if config.contains_key(&key)
            && let Ok(overrides) = config.section::<ServerConfig>(&key)
        {
            let port = overrides.port;
            self.server_config = overrides;
            if self.server_config.port == 0 {
                self.server_config.port = port;
            }
            tracing::debug!(group = %self.name, port = self.server_config.port, "rest group configured");
        }
    }
}

/// Builds a [`RestGroup`].
pub struct RestGroupBuilder {
    name: String,
    server_config: Option<ServerConfig>,
    registrations: Vec<Registration>,
}

impl RestGroupBuilder {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            server_config: None,
            registrations: Vec::new(),
        }
    }

    /// Sets the listening port. Shorthand for [`server_config`](Self::server_config).
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        let mut config = self.server_config.take().unwrap_or_default();
        config.port = port;
        self.server_config = Some(config);
        self
    }

    /// Sets the full server configuration.
    #[must_use]
    pub fn server_config(mut self, config: ServerConfig) -> Self {
        self.server_config = Some(config);
        self
    }

    /// Adds a controller.
    #[must_use]
    pub fn rest<R: Rest>(self, rest: R) -> Self {
        self.push(Registration::rest(rest))
    }

    /// Adds several controllers.
    #[must_use]
    pub fn rests<I, R>(mut self, rests: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: Rest,
    {
        for rest in rests {
            self = self.rest(rest);
        }
        self
    }

    /// Adds a filter that runs for every route in this group.
    #[must_use]
    pub fn filter<F: Filter + Send + Sync + 'static>(self, filter: F) -> Self {
        self.push(Registration::filter(filter))
    }

    /// Registers routes with a closure instead of a controller type.
    ///
    /// ```
    /// # use rust_web_frame::core::RestGroup;
    /// # use rust_web_frame::web::handler;
    /// let group = RestGroup::builder("public")
    ///     .routes(|ctx| {
    ///         ctx.get("/", handler(|_| async { Ok("hello world") }));
    ///     })
    ///     .build();
    /// assert_eq!(group.port(), 19009);
    /// ```
    #[must_use]
    pub fn routes<F>(self, register: F) -> Self
    where
        F: Fn(&Context) + Send + Sync + 'static,
    {
        self.push(Registration::rest(ClosureRest(register)))
    }

    /// Sets the response converter for this group.
    #[must_use]
    pub fn converter<C: Converter + Service>(self, converter: C) -> Self {
        self.push(Registration::converter(converter))
    }

    fn push(mut self, registration: Registration) -> Self {
        self.registrations.push(registration);
        self
    }

    /// Finishes the group.
    #[must_use]
    pub fn build(self) -> RestGroup {
        RestGroup {
            name: self.name,
            server_config: self.server_config.unwrap_or_default(),
            registrations: self.registrations,
        }
    }
}

/// A controller written as a closure.
struct ClosureRest<F>(F);

#[async_trait]
impl<F> Rest for ClosureRest<F>
where
    F: Fn(&Context) + Send + Sync + 'static,
{
    async fn init(&self, ctx: &Context) -> Result<()> {
        (self.0)(ctx);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "closure routes"
    }
}

/// Validates that no two groups claim the same port.
pub(crate) fn ensure_distinct_ports(groups: &[RestGroup], default_port: u16) -> Result<()> {
    let mut seen = vec![default_port];
    for group in groups {
        if group.port() == 0 {
            return Err(Error::new(
                500,
                format!("rest group `{}` has no port", group.name()),
            ));
        }
        if seen.contains(&group.port()) {
            return Err(Error::new(
                500,
                format!(
                    "rest group `{}` and another server both claim port {}",
                    group.name(),
                    group.port()
                ),
            ));
        }
        seen.push(group.port());
    }
    Ok(())
}

/// The config section that overrides rest group ports.
pub const REST_GROUPS_CONFIG_KEY: &str = "web.restGroups";

/// Reads the default server configuration out of the application config.
pub(crate) fn default_server_config(config: &crate::config::Config) -> ServerConfig {
    if config.contains_key(SERVER_CONFIG_KEY) {
        config
            .section::<ServerConfig>(SERVER_CONFIG_KEY)
            .unwrap_or_default()
    } else {
        ServerConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_routes_build_a_group_on_the_default_port() {
        let group = RestGroup::builder("public").routes(|_ctx| {}).build();
        assert_eq!(group.name(), "public");
        assert_eq!(group.port(), crate::web::DEFAULT_PORT);
    }

    #[test]
    fn ports_must_be_unique() {
        let groups = vec![
            RestGroup::builder("a").port(8081).build(),
            RestGroup::builder("b").port(8081).build(),
        ];
        let err = ensure_distinct_ports(&groups, 19009).unwrap_err();
        assert!(err.client_message().contains("8081"));

        let ok = vec![
            RestGroup::builder("a").port(8081).build(),
            RestGroup::builder("b").port(8082).build(),
        ];
        assert!(ensure_distinct_ports(&ok, 19009).is_ok());
    }

    #[test]
    fn the_default_port_cannot_be_claimed_twice() {
        let groups = vec![RestGroup::builder("dup").port(19009).build()];
        assert!(ensure_distinct_ports(&groups, 19009).is_err());
    }

    #[test]
    fn config_can_move_a_group_to_another_port() {
        let mut group = RestGroup::builder("admin").port(8081).build();
        let config = crate::config::Config::from_value(serde_json::json!({
            "web": {"restGroups": {"admin": {"port": 9091, "contextPath": "/admin"}}}
        }));
        group.apply_config(&config);
        assert_eq!(group.port(), 9091);
        assert_eq!(group.server_config().context_path, "/admin");
    }
}
