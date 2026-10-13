//! Authentication: an [`Authentication`] provider behind a filter that reads route
//! metadata.
//!
//! The shape is the framework's usual one. A route declares that it needs a signed-in
//! caller; one filter enforces it everywhere, so protecting a new endpoint is one call at
//! its route rather than a check every handler has to remember:
//!
//! ```no_run
//! # use rust_web_frame::component::auth::with_login;
//! # use rust_web_frame::core::Context;
//! # use rust_web_frame::web::handler;
//! # fn routes(ctx: &Context) {
//! ctx.get("/api/profile", handler(|_| async { Ok("profile") })).with_meta(with_login());
//! ctx.get("/api/login", handler(|_| async { Ok("token") }));   // public
//! # }
//! ```
//!
//! [`JwtAuth`] is a working provider — HS256 tokens, a configurable claim that names the
//! caller, and an optional issuer and audience — so an application that wants the obvious
//! thing writes no authentication code at all. One that wants something else implements
//! [`Authentication`] and registers that instead.

use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::{Context, Service};
use crate::web::{Error, Filter, FilterChain, MetaOption, Request, Response, Result};

/// Config section that holds the authentication settings.
pub const AUTH_CONFIG_KEY: &str = "auth";

/// The metadata key that marks a route as needing a signed-in caller.
pub const LOGIN_KEY: &str = "login";

/// Marks a route as needing a signed-in caller.
#[must_use]
pub fn with_login() -> MetaOption {
    MetaOption::value(LOGIN_KEY, true)
}

/// Marks a route as reachable without one, for the routes a caller uses to *become*
/// signed in.
#[must_use]
pub fn without_login() -> MetaOption {
    MetaOption::value("skip_login", true)
}

/// Who a request is from.
///
/// Deliberately not a generic: a filter has to hold the provider as a trait object, and a
/// generic user type would make that impossible. Claims that vary by application go in
/// [`claims`](Self::claims), which is what a handler reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    /// The caller's identifier — a user id, a service name.
    pub subject: String,
    /// When the token stops being valid, as a Unix timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Everything else the issuer put in the token.
    #[serde(flatten)]
    pub claims: Map<String, Value>,
}

impl Identity {
    /// An identity with the given subject.
    #[must_use]
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            ..Self::default()
        }
    }

    /// Reads a claim as a string.
    #[must_use]
    pub fn claim_str(&self, key: &str) -> Option<&str> {
        self.claims.get(key).and_then(Value::as_str)
    }

    /// Reads a claim as a boolean, which is what a permission flag usually is.
    #[must_use]
    pub fn claim_bool(&self, key: &str) -> bool {
        self.claims
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Reads a claim as a list of strings.
    #[must_use]
    pub fn claim_list(&self, key: &str) -> Vec<&str> {
        self.claims
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Reports whether a claim holds `wanted`, whether it is a list or a single value.
    ///
    /// This is what a permission check needs, and it is where an authorization bug lives if
    /// it is written by hand at each call site.
    #[must_use]
    pub fn has(&self, key: &str, wanted: &str) -> bool {
        match self.claims.get(key) {
            Some(Value::String(value)) => value == wanted,
            Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(wanted)),
            _ => false,
        }
    }
}

/// Where the filter gets an identity from.
///
/// The trait is the extension point: the implementation that ships covers the common case —
/// a signed token in a header — without the application writing anything, and anything else
/// (a session cookie, an opaque token store) remains a small impl.
#[async_trait]
pub trait Authentication: Send + Sync + 'static {
    /// Identifies the caller, or `Ok(None)` when the request carries no credentials.
    ///
    /// `Err` is for credentials that are *present and wrong* — an expired token, a bad
    /// signature — which is a different answer from "no token at all".
    ///
    /// # Errors
    ///
    /// Fails when the presented credentials are invalid.
    async fn authenticate(&self, req: &Request) -> Result<Option<Identity>>;

    /// Answers a sign-in. The default says the application has no sign-in endpoint.
    ///
    /// # Errors
    ///
    /// Whatever the provider needs to report.
    async fn sign_in(&self, _req: &Request) -> Result<Response> {
        Err(Error::not_found().detail("this application has no sign-in endpoint"))
    }

    /// Answers a sign-out. The default says the application has no sign-out endpoint.
    ///
    /// # Errors
    ///
    /// Whatever the provider needs to report.
    async fn sign_out(&self, _req: &Request) -> Result<Response> {
        Err(Error::not_found().detail("this application has no sign-out endpoint"))
    }
}

/// Where the identity is kept on the request, between the filter and the handler.
const IDENTITY_ATTR: &str = "rust-web-frame.identity";

/// The identity of the current request, when it has one.
///
/// # Errors
///
/// Fails when the route did not ask for a signed-in caller.
pub fn current_identity(req: &Request) -> Result<Arc<Identity>> {
    req.attr::<Identity>(IDENTITY_ATTR).ok_or_else(|| {
        Error::unauthorized().detail("this route has no identity — tag it with `with_login()`")
    })
}

/// Enforces [`with_login`] on the routes that carry it.
pub struct AuthFilter {
    provider: Arc<dyn Authentication>,
}

impl std::fmt::Debug for AuthFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthFilter").finish_non_exhaustive()
    }
}

impl AuthFilter {
    /// A filter over the given provider.
    #[must_use]
    pub fn new(provider: Arc<dyn Authentication>) -> Self {
        Self { provider }
    }

    /// The provider behind this filter.
    #[must_use]
    pub fn provider(&self) -> &Arc<dyn Authentication> {
        &self.provider
    }
}

#[async_trait]
impl Service for AuthFilter {
    async fn init(&self, ctx: &Context) -> Result<()> {
        // Wrapped once more so the trait object is `Sized` enough to store.
        ctx.register(Arc::new(Arc::clone(&self.provider)));
        Ok(())
    }
}

#[async_trait]
impl Filter for AuthFilter {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        // A route that never asked is not the filter's business, and neither is one that
        // opted out explicitly.
        if !req.has_meta(with_login()) || req.has_meta(without_login()) {
            return chain.next(req).await;
        }

        match self.provider.authenticate(&req).await? {
            Some(identity) => {
                // The handler reads this back with `current_identity`.
                req.set_attr(IDENTITY_ATTR, identity);
                chain.next(req).await
            }
            None => Err(Error::unauthorized().detail("this route needs a signed-in caller")),
        }
    }
}

/// JWT settings, read from the `auth` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct JwtConfig {
    /// The signing secret for HS256. Required.
    pub secret: String,
    /// The header the token arrives in.
    pub header: String,
    /// A prefix to strip from the header value, `Bearer ` by default.
    pub prefix: String,
    /// The claim that names the caller. `sub` unless the issuer says otherwise.
    pub subject_claim: String,
    /// Accepted issuers. Empty accepts any.
    pub issuers: Vec<String>,
    /// Accepted audiences. Empty accepts any.
    pub audiences: Vec<String>,
    /// Leeway in seconds when checking `exp` and `nbf`, for clock drift.
    pub leeway_seconds: u64,
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            secret: String::new(),
            header: String::from("authorization"),
            prefix: String::from("Bearer "),
            subject_claim: String::from("sub"),
            issuers: Vec::new(),
            audiences: Vec::new(),
            leeway_seconds: 30,
        }
    }
}

/// An [`Authentication`] provider backed by HS256 JSON Web Tokens.
///
/// Shared secrets rather than a key pair: a web application's own front end is the only
/// issuer in the common case, and an HMAC secret is one line of configuration where a key
/// pair is two files to distribute and rotate.
pub struct JwtAuth {
    config: JwtConfig,
    encoding: EncodingKey,
    decoding: DecodingKey,
    validation: Validation,
}

impl std::fmt::Debug for JwtAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtAuth")
            .field("header", &self.config.header)
            .field("subjectClaim", &self.config.subject_claim)
            .finish_non_exhaustive()
    }
}

impl JwtAuth {
    /// Builds a provider from its configuration.
    ///
    /// # Errors
    ///
    /// Fails when no secret is configured — an authentication provider with no secret
    /// accepts everything, which is worse than not starting.
    pub fn new(config: JwtConfig) -> Result<Self> {
        if config.secret.is_empty() {
            return Err(Error::new(500, "`auth.secret` is not set").detail(
                "JWT authentication needs a signing secret; refusing to start with an empty one",
            ));
        }

        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = config.leeway_seconds;
        if !config.issuers.is_empty() {
            validation.set_issuer(&config.issuers);
        }
        if !config.audiences.is_empty() {
            validation.set_audience(&config.audiences);
        }
        // The subject claim is read out of the decoded payload rather than validated here,
        // because a non-standard claim name is the whole reason this is configurable.
        validation.required_spec_claims.clear();
        validation.validate_exp = true;

        Ok(Self {
            encoding: EncodingKey::from_secret(config.secret.as_bytes()),
            decoding: DecodingKey::from_secret(config.secret.as_bytes()),
            validation,
            config,
        })
    }

    /// Signs an identity into a token.
    ///
    /// # Errors
    ///
    /// Fails when the claims cannot be encoded.
    pub fn issue(&self, identity: &Identity) -> Result<String> {
        let mut claims = identity.claims.clone();
        claims.insert(
            self.config.subject_claim.clone(),
            Value::String(identity.subject.clone()),
        );
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|err| Error::with_source(500, err).detail("cannot sign the token"))
    }

    /// Verifies a token and reads the identity out of it.
    ///
    /// # Errors
    ///
    /// Fails when the signature, the expiry or the issuer does not check out.
    pub fn verify(&self, token: &str) -> Result<Identity> {
        let decoded =
            jsonwebtoken::decode::<Map<String, Value>>(token, &self.decoding, &self.validation)
                .map_err(|err| {
                    // The reason is for the log; the client is told only that the token is no good.
                    tracing::debug!(error = %err, "token rejected");
                    match err.kind() {
                        jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                            Error::token_expired().detail("the token has expired")
                        }
                        _ => Error::token_invalid().detail("the token is not valid"),
                    }
                })?;

        let mut claims = decoded.claims;
        let subject = claims
            .remove(&self.config.subject_claim)
            .map(|value| match value {
                Value::String(text) => text,
                other => other.to_string(),
            })
            .ok_or_else(|| {
                Error::token_invalid().detail(format!(
                    "the token has no `{}` claim",
                    self.config.subject_claim
                ))
            })?;

        let expires_at = claims.remove("exp").and_then(|value| value.as_i64());

        Ok(Identity {
            subject,
            expires_at,
            claims,
        })
    }

    fn token_from(&self, req: &Request) -> Option<String> {
        let raw = req.header(&self.config.header)?;
        let token = match raw.strip_prefix(self.config.prefix.as_str()) {
            Some(rest) => rest,
            // A header with no prefix is accepted as the token itself, which is what a
            // service-to-service call usually sends.
            None => raw,
        };
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }
}

#[async_trait]
impl Authentication for JwtAuth {
    async fn authenticate(&self, req: &Request) -> Result<Option<Identity>> {
        match self.token_from(req) {
            Some(token) => self.verify(&token).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> JwtAuth {
        JwtAuth::new(JwtConfig {
            secret: String::from("a-test-secret-that-is-long-enough"),
            ..JwtConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn an_identity_round_trips_through_a_token() {
        let auth = auth();
        let mut identity = Identity::new("user-7");
        identity
            .claims
            .insert("roles".to_owned(), serde_json::json!(["admin", "editor"]));
        identity
            .claims
            .insert("staff".to_owned(), Value::Bool(true));

        let token = auth.issue(&identity).unwrap();
        let decoded = auth.verify(&token).unwrap();

        assert_eq!(decoded.subject, "user-7");
        assert!(decoded.has("roles", "admin"));
        assert!(decoded.has("roles", "editor"));
        assert!(!decoded.has("roles", "owner"));
        assert!(decoded.claim_bool("staff"));
        assert_eq!(decoded.claim_list("roles"), vec!["admin", "editor"]);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let mine = auth();
        let theirs = JwtAuth::new(JwtConfig {
            secret: String::from("someone-elses-secret-entirely"),
            ..JwtConfig::default()
        })
        .unwrap();

        let token = theirs.issue(&Identity::new("intruder")).unwrap();
        let error = mine.verify(&token).unwrap_err();

        assert_eq!(error.code, crate::web::code::TOKEN_INVALID);
        assert_eq!(
            error.http_status(),
            200,
            "a business code, not a transport failure"
        );
    }

    #[test]
    fn a_tampered_token_is_refused() {
        let auth = auth();
        let token = auth.issue(&Identity::new("user-7")).unwrap();

        // Flip a character in the payload.
        let mut tampered = token.clone();
        let position = tampered.len() / 2;
        let replacement = if tampered.as_bytes()[position] == b'A' {
            'B'
        } else {
            'A'
        };
        tampered.replace_range(position..position + 1, &replacement.to_string());

        assert!(auth.verify(&tampered).is_err());
    }

    #[test]
    fn an_expired_token_says_so() {
        let auth = auth();
        let mut identity = Identity::new("user-7");
        identity
            .claims
            .insert("exp".to_owned(), Value::from(1_000_000_000i64));

        let token = auth.issue(&identity).unwrap();
        let error = auth.verify(&token).unwrap_err();

        assert_eq!(
            error.code,
            crate::web::code::TOKEN_EXPIRED,
            "an expired token is a different answer from a forged one"
        );
    }

    #[test]
    fn an_issuer_can_be_required() {
        let strict = JwtAuth::new(JwtConfig {
            secret: String::from("a-test-secret-that-is-long-enough"),
            issuers: vec![String::from("https://expected.example.com")],
            ..JwtConfig::default()
        })
        .unwrap();

        let mine = auth();
        let mut wrong = Identity::new("user-7");
        wrong.claims.insert(
            "iss".to_owned(),
            Value::String("https://somewhere.else".to_owned()),
        );
        assert!(strict.verify(&mine.issue(&wrong).unwrap()).is_err());

        let mut right = Identity::new("user-7");
        right.claims.insert(
            "iss".to_owned(),
            Value::String("https://expected.example.com".to_owned()),
        );
        assert!(strict.verify(&mine.issue(&right).unwrap()).is_ok());
    }

    #[test]
    fn the_subject_claim_can_be_renamed() {
        let auth = JwtAuth::new(JwtConfig {
            secret: String::from("a-test-secret-that-is-long-enough"),
            subject_claim: String::from("user_id"),
            ..JwtConfig::default()
        })
        .unwrap();

        let token = auth.issue(&Identity::new("42")).unwrap();
        let decoded = auth.verify(&token).unwrap();
        assert_eq!(decoded.subject, "42");
        assert!(
            decoded.claims.get("user_id").is_none(),
            "the subject is lifted out rather than left in the claims"
        );

        // A token without the configured claim is refused rather than yielding an empty
        // subject, which would look like a valid caller called "".
        let bare = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &serde_json::json!({"sub": "42"}),
            &EncodingKey::from_secret(b"a-test-secret-that-is-long-enough"),
        )
        .unwrap();
        assert!(auth.verify(&bare).is_err());
    }

    #[test]
    fn an_empty_secret_refuses_to_start() {
        let error = JwtAuth::new(JwtConfig::default()).unwrap_err();
        assert!(error.message.contains("auth.secret"), "{}", error.message);
    }

    #[tokio::test]
    async fn the_filter_lets_an_unmarked_route_through() {
        let filter = AuthFilter::new(Arc::new(auth()));
        let mut chain = crate::web::FilterChain::new(
            Arc::new(Vec::new()),
            crate::web::handler(|_| async { Ok::<_, Error>(Response::text("reached")) }),
        );

        // A route with no metadata is not the filter's business.
        let response = filter.handle(&mut chain, Request::for_test("/open")).await;
        assert!(response.is_ok());
    }
}
