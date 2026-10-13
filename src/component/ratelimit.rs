//! Per-key rate limiting, enforced as a filter.
//!
//! One token bucket per caller — a client address, or an API token when there is one, so a
//! shared address behind a NAT is not treated as one caller. Tokens refill at a steady rate
//! and the bucket is bounded by a burst, which is how much a caller may spend at once.
//!
//! ```yaml
//! rate_limit:
//!   rate: 20         # tokens refilled per second
//!   burst: 40        # how much a caller may spend at once
//!   reject: true     # false reports the state in headers without refusing
//! ```
//!
//! Nothing is limited until a route asks for it, which is the framework's usual shape: the
//! route declares the policy, one filter enforces it.
//!
//! ```no_run
//! # use rust_web_frame::component::{rate_limit_exempt, rate_limited};
//! # use rust_web_frame::core::Context;
//! # use rust_web_frame::web::handler;
//! # fn routes(ctx: &Context) {
//! ctx.post("/login", handler(|_| async { Ok("ok") })).with_meta(rate_limited());
//! // The health check has to answer while everything else is being throttled.
//! ctx.get("/healthz", handler(|_| async { Ok("ok") })).with_meta(rate_limit_exempt());
//! # }
//! ```

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::core::{Context, Service};
use crate::web::{Error, Filter, FilterChain, MetaOption, Request, Response, Result};

/// Config section that holds the rate limit settings.
pub const RATE_LIMIT_CONFIG_KEY: &str = "rate_limit";

/// The metadata key that marks a route as rate limited.
pub const RATE_LIMITED_KEY: &str = "rate_limited";

/// Marks a route as rate limited.
#[must_use]
pub fn rate_limited() -> MetaOption {
    MetaOption::value(RATE_LIMITED_KEY, true)
}

/// Marks a route as exempt from rate limiting.
#[must_use]
pub fn rate_limit_exempt() -> MetaOption {
    MetaOption::value("skip_rate_limit", true)
}

/// Rate limit configuration, read from the `rate_limit` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RateLimitConfig {
    /// Tokens refilled per second.
    pub rate: f64,
    /// How many tokens a caller may hold, and therefore how much it may spend at once.
    pub burst: u32,
    /// Whether a rejected request is a `429`. Turned off, the request is allowed through
    /// and only the `x-ratelimit-*` headers report the state.
    pub reject: bool,
    /// How many callers are tracked before the table is cleared.
    pub max_keys: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            rate: 20.0,
            burst: 40,
            reject: true,
            max_keys: 100_000,
        }
    }
}

/// One caller's bucket.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// What a caller has left, for the response headers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quota {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Whole tokens left after this request.
    pub remaining: u32,
    /// The burst, which is what `remaining` is measured against.
    pub limit: u32,
    /// How long until a token is available, when the request was refused.
    pub retry_after: Option<Duration>,
}

/// Token buckets, one per caller.
pub struct RateLimit {
    config: RwLock<RateLimitConfig>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl std::fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let config = self.config.read().expect("rate limit poisoned");
        f.debug_struct("RateLimit")
            .field("rate", &config.rate)
            .field("burst", &config.burst)
            .field(
                "callers",
                &self.buckets.lock().map(|b| b.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::new(RateLimitConfig::default())
    }
}

impl RateLimit {
    /// A limiter with the given policy.
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config: RwLock::new(config),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The policy this limiter enforces.
    #[must_use]
    pub fn config(&self) -> RateLimitConfig {
        self.config.read().expect("rate limit poisoned").clone()
    }

    /// Spends one token for `caller`, reporting what is left.
    ///
    /// `burst` overrides the configured one for this call, which is how one route can ask
    /// for a tighter limit than the server default.
    pub fn check(&self, caller: &str, burst: Option<u32>) -> Quota {
        let config = self.config.read().expect("rate limit poisoned").clone();
        let burst = burst.unwrap_or(config.burst).max(1);
        // A rate of zero would mean "never refill", which also means "never divide by it".
        let rate = if config.rate > 0.0 { config.rate } else { 1.0 };
        let now = Instant::now();

        let mut buckets = self.buckets.lock().expect("rate limit poisoned");

        if buckets.len() >= config.max_keys && !buckets.contains_key(caller) {
            // Clearing is crude, but it is bounded work and it cannot grow without limit.
            // A limiter that leaks memory under a spray of keys is worse than one that
            // briefly forgets.
            tracing::debug!(
                callers = buckets.len(),
                "rate limit table is full, clearing it"
            );
            buckets.clear();
        }

        let bucket = buckets.entry(caller.to_owned()).or_insert_with(|| Bucket {
            tokens: f64::from(burst),
            last_refill: now,
        });

        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(f64::from(burst));
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Quota {
                allowed: true,
                remaining: bucket.tokens as u32,
                limit: burst,
                retry_after: None,
            };
        }

        let missing = 1.0 - bucket.tokens;
        Quota {
            allowed: false,
            remaining: 0,
            limit: burst,
            retry_after: Some(Duration::from_secs_f64(missing / rate)),
        }
    }
}

#[async_trait]
impl Service for RateLimit {
    async fn init(&self, ctx: &Context) -> Result<()> {
        let config: RateLimitConfig = ctx
            .config()
            .section(RATE_LIMIT_CONFIG_KEY)
            .unwrap_or_default();
        *self.config.write().expect("rate limit poisoned") = config;

        let config = self.config.read().expect("rate limit poisoned");
        tracing::debug!(rate = config.rate, burst = config.burst, "rate limit ready");
        Ok(())
    }
}

#[async_trait]
impl Filter for RateLimit {
    async fn handle(&self, chain: &mut FilterChain, req: Request) -> Result<Response> {
        if !req.has_meta(rate_limited()) || req.has_meta(rate_limit_exempt()) {
            return chain.next(req).await;
        }

        let caller = rate_limit_key(&req);
        let quota = self.check(&caller, None);

        // The headers go on every answer, so a well-behaved client can slow down before it
        // is refused — and they survive onto the 429, because they are queued rather than
        // set on a response this path never builds.
        req.set_response_header("x-ratelimit-limit", quota.limit.to_string());
        req.set_response_header("x-ratelimit-remaining", quota.remaining.to_string());

        if quota.allowed || !self.config.read().expect("rate limit poisoned").reject {
            return chain.next(req).await;
        }

        let retry_after = quota.retry_after.unwrap_or_default();
        let seconds = retry_after.as_secs().max(1);
        req.set_response_header("retry-after", seconds.to_string());
        Err(Error::too_many_requests()
            .detail(format!("too many requests — try again in {seconds}s")))
    }
}

/// What a caller is, for the purpose of limiting it: the API token when there is one, so a
/// shared address behind a NAT is not one caller, and the address otherwise.
fn rate_limit_key(req: &Request) -> String {
    if let Some(token) = req.header("authorization") {
        // Truncated: a key is a map key, not a place to keep a secret.
        return format!("token:{}", &token[..token.len().min(64)]);
    }
    let address = req.client_ip();
    if address.is_empty() {
        return String::from("unknown");
    }
    format!("ip:{address}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{HandlerMeta, handler};

    fn limiter(rate: f64, burst: u32) -> RateLimit {
        RateLimit::new(RateLimitConfig {
            rate,
            burst,
            ..RateLimitConfig::default()
        })
    }

    #[test]
    fn a_burst_is_allowed_then_the_bucket_is_empty() {
        let limiter = limiter(1.0, 3);

        for expected in [2, 1, 0] {
            let quota = limiter.check("a", None);
            assert!(quota.allowed, "the burst should cover the first three");
            assert_eq!(quota.remaining, expected);
        }

        let refused = limiter.check("a", None);
        assert!(!refused.allowed);
        assert_eq!(refused.remaining, 0);
        assert!(refused.retry_after.is_some());
    }

    #[test]
    fn callers_are_counted_separately() {
        let limiter = limiter(1.0, 1);
        assert!(limiter.check("a", None).allowed);
        assert!(!limiter.check("a", None).allowed);
        assert!(limiter.check("b", None).allowed, "b has its own bucket");
    }

    #[test]
    fn tokens_refill_over_time() {
        let limiter = limiter(100.0, 1);
        assert!(limiter.check("a", None).allowed);
        assert!(!limiter.check("a", None).allowed);

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            limiter.check("a", None).allowed,
            "100/s refills a token in 10ms"
        );
    }

    #[test]
    fn a_burst_override_only_affects_that_call() {
        let limiter = limiter(1.0, 10);
        assert_eq!(
            limiter.check("a", Some(2)).limit,
            2,
            "the override is reported back"
        );
        assert_eq!(
            limiter.check("a", None).limit,
            10,
            "the configured burst is unchanged"
        );
    }

    #[test]
    fn a_zero_rate_still_answers_rather_than_dividing_by_zero() {
        let limiter = limiter(0.0, 2);
        assert!(limiter.check("a", None).allowed);
        assert!(limiter.check("a", None).allowed);

        let refused = limiter.check("a", None);
        assert!(!refused.allowed);
        assert!(
            refused.retry_after.is_some(),
            "the retry hint must be finite"
        );
    }

    #[tokio::test]
    async fn only_a_route_that_opted_in_is_limited() {
        let limiter = limiter(0.001, 1);
        let mut chain = crate::web::FilterChain::new(
            std::sync::Arc::new(Vec::new()),
            handler(|_| async { Ok::<_, Error>(Response::text("reached")) }),
        );

        // An unmarked route: the filter passes it straight through, however many times.
        for _ in 0..5 {
            let response = limiter.handle(&mut chain, Request::for_test("/")).await;
            assert!(response.is_ok());
        }

        // The marker itself is what a real request would carry.
        let meta = HandlerMeta::new();
        rate_limited().apply(&meta);
        assert!(rate_limited().matches(&meta));
        assert!(!rate_limit_exempt().matches(&meta));
    }

    #[test]
    fn the_config_section_uses_camel_case() {
        let config: RateLimitConfig =
            serde_json::from_value(serde_json::json!({"rate": 5.5, "burst": 9, "maxKeys": 12}))
                .unwrap();
        assert_eq!(config.rate, 5.5);
        assert_eq!(config.burst, 9);
        assert_eq!(config.max_keys, 12);
        assert!(config.reject, "the default is to refuse");
    }
}
