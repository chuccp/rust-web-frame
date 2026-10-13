//! Everything at once, so a component that only works alone is caught here.
//!
//! Registration goes through the [`Builder`], which is what actually puts a component in
//! the container — calling `init` by hand runs a component's startup but hands nothing to a
//! handler.

use std::sync::Arc;

use tower::ServiceExt;

use crate::Builder;
use crate::component::*;
use crate::config::Config;
use crate::web::{Message, Request, handler};

/// Builds the cache asynchronously, then hands it to the builder.
async fn cache() -> CacheService {
    CacheService::from_config(&CacheConfig::default())
        .await
        .expect("a memory cache needs no server")
}

async fn get(router: &axum::Router, path: &str) -> serde_json::Value {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("served");
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("readable");
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn a_handler_can_reach_every_component() {
    let cache = cache().await;
    let root = tempfile::tempdir().unwrap();

    let mut builder = Builder::new(Config::empty());
    builder.service(cache);
    builder.service(Validator::default());
    builder.service(RateLimit::default());
    builder.service(Captcha::default());
    builder.service(Proxy::default());
    builder.service(LocalCache::new(LocalCacheConfig {
        path: root.path().to_path_buf(),
        namespace: String::new(),
    }));
    builder.get(
        "/check",
        handler(|req: Request| async move {
            let cache = req.context().service::<Arc<dyn Cache>>()?;
            cache.set("seen", serde_json::json!(1), None).await;

            let validator = req.context().service::<Validator>()?;
            let weak = validator.password("password", "short");

            let limiter = req.context().service::<RateLimit>()?;
            let quota = limiter.check("test", None);

            let captcha = req.context().service::<Captcha>()?;
            let challenge = captcha.challenge()?;
            // Nonsense is refused, and a challenge is spent by being answered at all.
            let refused = captcha.validate(&challenge.id, "not the answer").is_err();
            let spent = captcha.validate(&challenge.id, "7").is_err();

            let proxy = req.context().service::<Proxy>()?;
            let forwardable = proxy.forward("/behind", "http://127.0.0.1:19099").is_ok();

            let local = req.context().service::<LocalCache>()?;

            Ok(Message::data(serde_json::json!({
                "cached": cache.get("seen").await.is_some(),
                "weakPasswordRefused": weak.is_err(),
                "firstRequestAllowed": quota.allowed,
                "questionAsked": !challenge.question.is_empty() && !challenge.id.is_empty(),
                "nonsenseRefused": refused,
                "challengeSpent": spent,
                "forwardable": forwardable,
                "cacheRooted": local.path().is_dir(),
            })))
        }),
    );

    let router = builder
        .build()
        .into_router()
        .await
        .expect("the application initialises");

    let json = get(&router, "/check").await;
    for field in [
        "cached",
        "weakPasswordRefused",
        "firstRequestAllowed",
        "questionAsked",
        "nonsenseRefused",
        "challengeSpent",
        "forwardable",
        "cacheRooted",
    ] {
        assert_eq!(json["data"][field], true, "{field}: {json}");
    }
}

#[tokio::test]
async fn the_rate_limit_filter_runs_alongside_them() {
    let cache = cache().await;

    let mut builder = Builder::new(Config::empty());
    builder.service(cache);
    builder.service(Validator::default());
    // One token in total, and the route opts in — so the second call is refused.
    builder.filter(RateLimit::new(RateLimitConfig {
        rate: 0.001,
        burst: 1,
        ..RateLimitConfig::default()
    }));
    builder
        .get(
            "/limited",
            handler(|_| async { Ok(Message::data("through")) }),
        )
        .with_meta(rate_limited());
    builder
        .get("/open", handler(|_| async { Ok(Message::data("through")) }))
        .with_meta(rate_limit_exempt());

    let router = builder
        .build()
        .into_router()
        .await
        .expect("the application initialises");

    let first = get(&router, "/limited").await;
    assert_eq!(first["data"], "through", "{first}");
    assert_eq!(first["code"], 200);

    let second = get(&router, "/limited").await;
    assert_eq!(
        second["code"], 429,
        "the second call must be refused: {second}"
    );
    assert!(second["msg"].as_str().unwrap().contains("again in"));

    // A route that opted out keeps answering.
    for _ in 0..3 {
        let open = get(&router, "/open").await;
        assert_eq!(open["data"], "through", "{open}");
    }
}

#[tokio::test]
async fn a_validation_failure_keeps_its_business_code() {
    let mut builder = Builder::new(Config::empty());
    builder.service(Validator::default());
    builder.get(
        "/signup",
        handler(|req: Request| async move {
            let validator = req.context().service::<Validator>()?;
            validator.mobile("mobile", req.query_or("mobile", ""))?;
            Ok(Message::with_message("ok"))
        }),
    );

    let router = builder
        .build()
        .into_router()
        .await
        .expect("the application initialises");

    let bad = get(&router, "/signup?mobile=nonsense").await;
    assert_eq!(bad["code"], 2002, "{bad}");
    // A business code travels over HTTP 200, like every other one.
    assert_ne!(bad["msg"], "ok");

    let good = get(&router, "/signup?mobile=13800138000").await;
    assert_eq!(good["msg"], "ok", "{good}");
}
