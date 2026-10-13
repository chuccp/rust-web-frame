//! Arithmetic captchas, and the challenge behind them.
//!
//! A challenge is two small numbers and an operator — `3 + 7` — and the answer stays here,
//! on the server, rather than travelling to the client inside a token. The id the caller
//! gets back is opaque: reading it reveals nothing, and answering spends it.
//!
//! ```no_run
//! use rust_web_frame::component::Captcha;
//! use rust_web_frame::web::{Message, Request, Result};
//!
//! async fn new_challenge(req: Request) -> Result<Message> {
//!     let captcha = req.context().service::<Captcha>()?;
//!     let challenge = captcha.challenge()?;
//!     Ok(Message::data(serde_json::json!({
//!         "id": challenge.id,
//!         "question": challenge.question,
//!         "expiresIn": challenge.expires_in,
//!     })))
//! }
//!
//! async fn sign_up(req: Request) -> Result<Message> {
//!     let captcha = req.context().service::<Captcha>()?;
//!     let id = req.json_str_or("captchaId", "")?;
//!     let answer = req.json_str_or("captchaAnswer", "")?;
//!     captcha.validate(&id, &answer)?;
//!     Ok(Message::with_message("registered"))
//! }
//! ```
//!
//! ```yaml
//! captcha:
//!   ttl: 300            # seconds an unanswered challenge stays valid
//!   maxPending: 10000   # how many unanswered challenges are remembered
//!   maxOperand: 9       # the largest number a question uses
//! ```
//!
//! Guarding a form is the handler's job — [`validate`](Captcha::validate) is one call,
//! skipped when a caller is already signed in, which is not a decision a filter can make.
//!
//! # Where the challenges live
//!
//! In this process, in a bounded map whose entries expire. Nothing is written to the
//! database, and an abandoned challenge costs one entry until its ttl passes. Two instances
//! behind a load balancer do not share them, so issue and validate on the same one.

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use moka::sync::Cache as Expiring;
use serde::{Deserialize, Serialize};

use crate::core::{Context, Service};
use crate::web::{Error, Result};

/// Config section that holds the captcha settings.
pub const CAPTCHA_CONFIG_KEY: &str = "captcha";

/// Business codes this component raises.
pub mod code {
    /// The challenge is unknown, already answered, or past its ttl. A caller should ask for
    /// a new one; this is the code that tells a client to do that.
    pub const CAPTCHA_EXPIRED: i32 = 2101;
    /// The answer did not match the challenge.
    pub const CAPTCHA_INVALID: i32 = 2102;
}

/// Captcha configuration, read from the `captcha` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CaptchaConfig {
    /// How long an unanswered challenge stays valid, in seconds.
    pub ttl: u64,
    /// How many unanswered challenges are remembered. Past this the oldest are dropped, so
    /// a caller that asks for challenges and never answers them cannot grow the process.
    pub max_pending: u64,
    /// The largest number a question uses. Nine keeps the answer under a hundred even when
    /// the operator is `×`.
    pub max_operand: u32,
}

impl Default for CaptchaConfig {
    fn default() -> Self {
        Self {
            ttl: 300,
            max_pending: 10_000,
            max_operand: 9,
        }
    }
}

/// A challenge for the caller to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    /// The opaque id, to be sent back with the answer.
    pub id: String,
    /// What to show the caller, e.g. `3 + 7`.
    pub question: String,
    /// How long the answer will be accepted for, in seconds.
    pub expires_in: u64,
}

/// Arithmetic captchas: a question, an expiring challenge, and one answer each.
pub struct Captcha {
    /// Read from the `captcha` section at startup; behind a lock because
    /// [`init`](Service::init) takes `&self`.
    config: RwLock<CaptchaConfig>,
    /// id -> answer, built on first use from whatever the config says by then. A moka cache
    /// takes its ttl and its capacity when it is built, and that is one moment later than
    /// the config is known.
    pending: OnceLock<Expiring<String, i64>>,
}

impl std::fmt::Debug for Captcha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let config = self.settings();
        f.debug_struct("Captcha")
            .field("ttl", &config.ttl)
            .field("maxOperand", &config.max_operand)
            .field("pending", &self.pending.get().map(Expiring::entry_count))
            .finish()
    }
}

impl Default for Captcha {
    fn default() -> Self {
        Self::new(CaptchaConfig::default())
    }
}

impl Captcha {
    /// A captcha with the given settings.
    #[must_use]
    pub fn new(config: CaptchaConfig) -> Self {
        Self {
            config: RwLock::new(config),
            pending: OnceLock::new(),
        }
    }

    /// The settings in force.
    #[must_use]
    pub fn config(&self) -> CaptchaConfig {
        self.settings()
    }

    /// The settings in force, copied out so the lock is not held across anything else.
    fn settings(&self) -> CaptchaConfig {
        self.config.read().expect("captcha poisoned").clone()
    }

    /// The table the challenges wait in.
    fn store(&self) -> &Expiring<String, i64> {
        self.pending.get_or_init(|| {
            let config = self.settings();
            Expiring::builder()
                .time_to_live(Duration::from_secs(config.ttl))
                .max_capacity(config.max_pending)
                .build()
        })
    }

    /// Issues a challenge.
    ///
    /// The question is `a + b`, `a - b` or `a × b`, with both numbers in `1..=maxOperand`
    /// and a subtraction that never goes below zero. The answer is kept here until it is
    /// given, the ttl passes, or the table fills up.
    ///
    /// # Errors
    ///
    /// Fails when the operating system's random number generator does — without randomness
    /// there is no challenge to issue.
    pub fn challenge(&self) -> Result<Challenge> {
        let config = self.settings();
        let max = u64::from(config.max_operand.max(1));
        let first = roll(max)?;
        let second = roll(max)?;
        let (question, answer) = match roll(3)? {
            0 => (format!("{first} + {second}"), first + second),
            1 => {
                let (high, low) = if first >= second {
                    (first, second)
                } else {
                    (second, first)
                };
                (format!("{high} - {low}"), high - low)
            }
            // `saturating_mul` rather than `*`: a `maxOperand` large enough to overflow is a
            // misconfiguration, not a reason to panic.
            _ => (format!("{first} × {second}"), first.saturating_mul(second)),
        };

        let id = token()?;
        self.store().insert(id.clone(), answer as i64);
        Ok(Challenge {
            id,
            question,
            expires_in: config.ttl,
        })
    }

    /// Checks an answer, and spends the challenge doing it.
    ///
    /// The answer is accepted once, and an answer that is *wrong* spends the challenge too:
    /// the answer space of an arithmetic question is small enough to walk through, so a
    /// challenge that survived a failed attempt would be no challenge at all.
    ///
    /// # Errors
    ///
    /// [`code::CAPTCHA_EXPIRED`] when the id is unknown, already answered, or too old;
    /// [`code::CAPTCHA_INVALID`] when the answer is not the answer.
    pub fn validate(&self, id: &str, answer: &str) -> Result<()> {
        let expected = self.store().remove(id).ok_or_else(|| {
            Error::new(code::CAPTCHA_EXPIRED, "the captcha has expired").detail(
                "the challenge is unknown, already answered, or past its ttl — ask for a new one",
            )
        })?;

        let given = answer.trim().parse::<i64>().map_err(|_| wrong())?;
        if given == expected {
            Ok(())
        } else {
            Err(wrong())
        }
    }
}

/// The error an answer that is not the answer gets.
fn wrong() -> Error {
    Error::new(code::CAPTCHA_INVALID, "the captcha answer is wrong")
        .detail("answer the question that was asked, and answer it once")
}

/// A number in `1..=max`.
fn roll(max: u64) -> Result<u64> {
    getrandom::u64()
        .map(|value| value % max + 1)
        .map_err(no_entropy)
}

/// An opaque id: sixteen random bytes, url-safe, so it survives a query string.
fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(no_entropy)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The error a machine without an entropy source gets.
fn no_entropy(err: getrandom::Error) -> Error {
    Error::with_source(500, err).detail("the system's random number generator is unavailable")
}

#[async_trait]
impl Service for Captcha {
    /// Reads the `captcha` section into this instance.
    ///
    /// The registered component is the one the container keeps, so the settings are applied
    /// to it rather than to a second copy: a component that replaced itself here would
    /// leave `ctx.service::<Captcha>()` and the running captcha as two different objects.
    async fn init(&self, ctx: &Context) -> Result<()> {
        let config: CaptchaConfig = ctx.config().section(CAPTCHA_CONFIG_KEY).unwrap_or_default();

        tracing::debug!(
            ttl = config.ttl,
            maxOperand = config.max_operand,
            "captcha ready"
        );
        *self.config.write().expect("captcha poisoned") = config;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer to a question like `3 + 7`, computed the long way round so the test does
    /// not repeat the component's own arithmetic.
    fn solve(question: &str) -> i64 {
        let mut parts = question.split_whitespace();
        let left: i64 = parts.next().expect("a number").parse().expect("a number");
        let operator = parts.next().expect("an operator");
        let right: i64 = parts.next().expect("a number").parse().expect("a number");
        match operator {
            "+" => left + right,
            "-" => left - right,
            "×" => left * right,
            other => panic!("unexpected operator {other}"),
        }
    }

    fn captcha() -> Captcha {
        Captcha::new(CaptchaConfig::default())
    }

    #[test]
    fn a_challenge_is_a_question_with_a_small_answer() {
        let captcha = captcha();
        for _ in 0..200 {
            let challenge = captcha.challenge().unwrap();
            let answer = solve(&challenge.question);

            assert!(answer >= 0, "{} went negative", challenge.question);
            assert!(
                answer <= 81,
                "{} is larger than nine times nine",
                challenge.question
            );
            assert_eq!(challenge.expires_in, 300);
            assert!(!challenge.id.is_empty());
        }
    }

    #[test]
    fn the_right_answer_is_accepted_once() {
        let captcha = captcha();
        let challenge = captcha.challenge().unwrap();
        let answer = solve(&challenge.question).to_string();

        assert!(captcha.validate(&challenge.id, &answer).is_ok());
        // The same answer again, which is what a replay looks like.
        let again = captcha.validate(&challenge.id, &answer).unwrap_err();
        assert_eq!(again.code, code::CAPTCHA_EXPIRED);
    }

    #[test]
    fn a_wrong_answer_spends_the_challenge() {
        let captcha = captcha();
        let challenge = captcha.challenge().unwrap();
        let answer = solve(&challenge.question);

        let wrong_answer = captcha
            .validate(&challenge.id, &(answer + 1).to_string())
            .unwrap_err();
        assert_eq!(wrong_answer.code, code::CAPTCHA_INVALID);

        // Walking the whole answer space would otherwise be a way past the captcha.
        let second_try = captcha
            .validate(&challenge.id, &answer.to_string())
            .unwrap_err();
        assert_eq!(second_try.code, code::CAPTCHA_EXPIRED);
    }

    #[test]
    fn an_unknown_id_is_expired_rather_than_wrong() {
        let captcha = captcha();
        let refused = captcha.validate("never-issued", "7").unwrap_err();
        assert_eq!(refused.code, code::CAPTCHA_EXPIRED);
    }

    #[test]
    fn an_answer_that_is_not_a_number_is_refused() {
        let captcha = captcha();
        let challenge = captcha.challenge().unwrap();

        let refused = captcha.validate(&challenge.id, "seven").unwrap_err();
        assert_eq!(refused.code, code::CAPTCHA_INVALID);
    }

    #[test]
    fn the_operand_bound_is_the_configured_one() {
        let captcha = Captcha::new(CaptchaConfig {
            max_operand: 3,
            ..CaptchaConfig::default()
        });
        for _ in 0..100 {
            let challenge = captcha.challenge().unwrap();
            for token in challenge.question.split_whitespace().step_by(2) {
                let operand: u64 = token.parse().unwrap();
                assert!((1..=3).contains(&operand), "{}", challenge.question);
            }
        }
    }

    #[test]
    fn challenges_are_remembered_and_then_dropped() {
        // Nothing is answered here, so every challenge stays pending — up to the bound.
        let captcha = Captcha::new(CaptchaConfig {
            max_pending: 4,
            ..CaptchaConfig::default()
        });
        for _ in 0..8 {
            captcha.challenge().unwrap();
        }
        let pending = captcha.store();
        pending.run_pending_tasks();
        assert!(pending.entry_count() <= 4, "{}", pending.entry_count());
    }
}
