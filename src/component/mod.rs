//! Optional pieces that most applications want, built on the framework rather than into
//! it.
//!
//! Each one is an ordinary [`Service`](crate::core::Service) — or a
//! [`Filter`](crate::web::Filter) — so the extension points the framework already has are
//! the only thing that makes them work. Nothing here is privileged.
//!
//! | Component | What it adds |
//! |---|---|
//! | [`cache`] | one `Cache` trait, two backends: this process, or a shared Redis |
//! | [`localcache`] | a content-addressed directory of generated files |
//! | [`ratelimit`] | per-key token buckets, enforced as a filter |
//! | [`auth`] | an `Authentication` provider behind a filter that reads route metadata |
//! | [`schedule`] | cron jobs with key-based lifecycle |
//! | [`validator`] | field rules with stable error codes |
//! | [`qrcode`] | QR codes and data-URI images |
//! | [`captcha`] | arithmetic captchas, and the challenge behind them |
//! | [`mail`] | parsing and formatting addresses |
//! | [`proxy`] | forwarding a path to another service |

pub mod auth;
pub mod cache;
pub mod captcha;
pub mod localcache;
pub mod mail;
pub mod proxy;
pub mod qrcode;
pub mod ratelimit;
pub mod schedule;
pub mod validator;

pub use cache::{Backend, Cache, CacheConfig, CacheExt, CacheService, MemoryCache, RedisCache};
pub use captcha::{Captcha, CaptchaConfig, Challenge};
pub use localcache::{LocalCache, LocalCacheConfig};
pub use proxy::{Proxy, ProxyConfig};
pub use ratelimit::{
    Quota, RATE_LIMIT_CONFIG_KEY, RATE_LIMITED_KEY, RateLimit, RateLimitConfig, rate_limit_exempt,
    rate_limited,
};
pub use schedule::Schedule;
pub use validator::{ValidationError, Validator, ValidatorConfig};

#[cfg(test)]
mod smoke;
