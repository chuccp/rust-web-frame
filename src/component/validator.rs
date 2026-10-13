//! Field validation with stable error codes.
//!
//! The rules a registration form actually needs — a mobile number, a password with some
//! strength to it — plus the plumbing to report *which* field failed without leaking what
//! the rule is.
//!
//! ```no_run
//! use rust_web_frame::component::Validator;
//! use rust_web_frame::web::{Error, Message, Request, Result};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct SignUp {
//!     mobile: String,
//!     password: String,
//! }
//!
//! async fn sign_up(req: Request) -> Result<Message> {
//!     let payload: SignUp = req.bind_json()?;
//!     let validator = req.context().service::<Validator>()?;
//!
//!     validator.mobile("mobile", &payload.mobile)?;
//!     validator.password("password", &payload.password)?;
//!     Ok(Message::with_message("registered"))
//! }
//! ```
//!
//! Validation is called from the handler rather than derived from annotations, because Rust
//! has no runtime attribute reflection and a derive macro for rules would be a second
//! language to learn. The rules are ordinary functions, so a project-specific one is a
//! three-line addition.

use std::sync::RwLock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::core::{Context, Service};
use crate::web::{Error, Result};

/// Config section that holds the validation settings.
pub const VALIDATOR_CONFIG_KEY: &str = "validator";

/// A validation failure: a stable code, a message, and the field that failed.
///
/// The code is what a client branches on; the message is for a human; the field is what lets
/// a form highlight the right input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationError {
    /// A business code, in the same space as [`crate::web::code`].
    pub code: i32,
    /// What went wrong, in words a user can act on.
    pub message: String,
    /// The field that failed, when the rule knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// Business codes this component raises.
pub mod code {
    /// A password that does not meet the strength rule.
    pub const PASSWORD_REQUIRED: i32 = 2001;
    /// A mobile number that is not one.
    pub const MOBILE_INVALID: i32 = 2002;
    /// Any other field rule.
    pub const FIELD_INVALID: i32 = 2003;
}

impl ValidationError {
    /// A failure with the given code and message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
        }
    }

    /// Attaches the field that failed.
    #[must_use]
    pub fn on(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Turns this into the framework's error, so it can be returned with `?`.
    ///
    /// The code travels in the envelope, which is what a client checks; the HTTP status
    /// stays 200, like every other business code.
    #[must_use]
    pub fn into_error(self) -> Error {
        let detail = match &self.field {
            Some(field) => format!("{}: {}", field, self.message),
            None => self.message.clone(),
        };
        Error::new(self.code, self.message.clone()).detail(detail)
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.field {
            Some(field) => write!(f, "[{}] {}: {}", self.code, field, self.message),
            None => write!(f, "[{}] {}", self.code, self.message),
        }
    }
}

impl std::error::Error for ValidationError {}

impl From<ValidationError> for Error {
    fn from(value: ValidationError) -> Self {
        value.into_error()
    }
}

/// Validator configuration, read from the `validator` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ValidatorConfig {
    /// Shortest accepted password.
    pub min_password_length: usize,
    /// Whether a password needs an upper-case letter.
    pub require_uppercase: bool,
    /// Whether a password needs a lower-case letter.
    pub require_lowercase: bool,
    /// Whether a password needs a digit.
    pub require_digit: bool,
    /// Accepted mobile country code prefixes, tried before the bare 11-digit rule.
    pub mobile_prefixes: Vec<String>,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            min_password_length: 8,
            require_uppercase: true,
            require_lowercase: true,
            require_digit: true,
            mobile_prefixes: vec![String::from("+86"), String::from("86")],
        }
    }
}

/// The field rules.
pub struct Validator {
    config: RwLock<ValidatorConfig>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("config", &self.config.read().map(|c| c.clone()).ok())
            .finish()
    }
}

impl Default for Validator {
    fn default() -> Self {
        Self::new(ValidatorConfig::default())
    }
}

impl Validator {
    /// A validator with the given rules.
    #[must_use]
    pub fn new(config: ValidatorConfig) -> Self {
        Self {
            config: RwLock::new(config),
        }
    }

    /// The rules in force.
    #[must_use]
    pub fn config(&self) -> ValidatorConfig {
        self.config.read().expect("validator poisoned").clone()
    }

    /// Checks a password against the configured strength rule.
    ///
    /// # Errors
    ///
    /// Fails with [`code::PASSWORD_REQUIRED`]. The message says what the rule *is* — a user
    /// who cannot see the rule cannot satisfy it — but not which character class this
    /// particular password was missing, which would only help someone guessing.
    pub fn password(&self, field: &str, value: &str) -> Result<()> {
        let config = self.config.read().expect("validator poisoned");
        let value = value.trim();

        let long_enough = value.chars().count() >= config.min_password_length;
        let has_upper = !config.require_uppercase || value.chars().any(char::is_uppercase);
        let has_lower = !config.require_lowercase || value.chars().any(char::is_lowercase);
        let has_digit = !config.require_digit || value.chars().any(|c| c.is_ascii_digit());

        if long_enough && has_upper && has_lower && has_digit {
            return Ok(());
        }

        let mut wants = Vec::new();
        if config.require_uppercase {
            wants.push("an upper-case letter");
        }
        if config.require_lowercase {
            wants.push("a lower-case letter");
        }
        if config.require_digit {
            wants.push("a digit");
        }
        let shape = if wants.is_empty() {
            String::new()
        } else {
            format!(", including {}", wants.join(", "))
        };

        Err(ValidationError::new(
            code::PASSWORD_REQUIRED,
            format!(
                "a password needs at least {} characters{shape}",
                config.min_password_length
            ),
        )
        .on(field)
        .into())
    }

    /// Checks a mobile number.
    ///
    /// Spaces, dashes and a configured country prefix are stripped first, so the shapes a
    /// real form produces all pass.
    ///
    /// # Errors
    ///
    /// Fails with [`code::MOBILE_INVALID`].
    pub fn mobile(&self, field: &str, value: &str) -> Result<()> {
        let config = self.config.read().expect("validator poisoned");
        let mut digits: String = value
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '(' | ')'))
            .collect();

        for prefix in &config.mobile_prefixes {
            if let Some(rest) = digits.strip_prefix(prefix.as_str()) {
                digits = rest.to_owned();
                break;
            }
        }

        // Eleven digits starting with a one: the mainland China mobile shape, with the
        // prefixes above doing the country-code work.
        let valid = digits.len() == 11
            && digits.starts_with('1')
            && digits.chars().all(|c| c.is_ascii_digit());

        if valid {
            return Ok(());
        }
        Err(
            ValidationError::new(code::MOBILE_INVALID, "that is not a mobile number")
                .on(field)
                .into(),
        )
    }

    /// Checks an email address.
    ///
    /// Deliberately loose: the only authority on whether an address exists is the server
    /// that receives the mail, and a strict pattern rejects addresses that are legal.
    ///
    /// # Errors
    ///
    /// Fails with [`code::FIELD_INVALID`].
    pub fn email(&self, field: &str, value: &str) -> Result<()> {
        let candidate = value.trim();
        let shaped_right = match candidate.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !local.contains(char::is_whitespace)
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !domain.contains(char::is_whitespace)
            }
            None => false,
        };

        if shaped_right {
            return Ok(());
        }
        Err(
            ValidationError::new(code::FIELD_INVALID, "that is not an email address")
                .on(field)
                .into(),
        )
    }

    /// Checks that a string is neither empty nor only whitespace.
    ///
    /// # Errors
    ///
    /// Fails with [`code::FIELD_INVALID`].
    pub fn required(&self, field: &str, value: &str) -> Result<()> {
        if !value.trim().is_empty() {
            return Ok(());
        }
        Err(
            ValidationError::new(code::FIELD_INVALID, "this field is required")
                .on(field)
                .into(),
        )
    }

    /// Checks that a string fits in `max` characters.
    ///
    /// # Errors
    ///
    /// Fails with [`code::FIELD_INVALID`].
    pub fn max_length(&self, field: &str, value: &str, max: usize) -> Result<()> {
        let length = value.chars().count();
        if length <= max {
            return Ok(());
        }
        Err(ValidationError::new(
            code::FIELD_INVALID,
            format!("at most {max} characters, got {length}"),
        )
        .on(field)
        .into())
    }

    /// Checks a value against a rule of the caller's own.
    ///
    /// # Errors
    ///
    /// Fails with [`code::FIELD_INVALID`] when the rule returns false.
    pub fn check<F>(&self, field: &str, value: &str, rule: F, message: &str) -> Result<()>
    where
        F: FnOnce(&str) -> bool,
    {
        if rule(value) {
            return Ok(());
        }
        Err(ValidationError::new(code::FIELD_INVALID, message)
            .on(field)
            .into())
    }
}

#[async_trait]
impl Service for Validator {
    async fn init(&self, ctx: &Context) -> Result<()> {
        let config: ValidatorConfig = ctx
            .config()
            .section(VALIDATOR_CONFIG_KEY)
            .unwrap_or_default();
        *self.config.write().expect("validator poisoned") = config;
        tracing::debug!("validator ready");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> Validator {
        Validator::default()
    }

    #[test]
    fn a_strong_password_passes_and_a_weak_one_says_what_is_missing() {
        let validator = validator();
        assert!(validator.password("password", "Passw0rdd").is_ok());
        assert!(
            validator.password("password", "  Passw0rdd  ").is_ok(),
            "surrounding space is trimmed"
        );

        for weak in ["short1A", "alllowercase1", "ALLUPPERCASE1", "NoDigitsHere"] {
            let error = validator.password("password", weak).unwrap_err();
            assert_eq!(error.code, code::PASSWORD_REQUIRED, "{weak}");
        }
    }

    #[test]
    fn the_password_message_states_the_rule_without_grading_the_attempt() {
        let validator = validator();
        let error = validator.password("password", "short").unwrap_err();
        let message = error.client_message();

        assert!(
            message.contains("8"),
            "the length is part of the rule: {message}"
        );
        assert!(
            message.contains("password"),
            "the field is named: {message}"
        );
        assert!(
            !message.contains("upper-case letter is missing"),
            "the message must not say which part this attempt failed: {message}"
        );
    }

    #[test]
    fn a_password_rule_can_be_relaxed_by_configuration() {
        let validator = Validator::new(ValidatorConfig {
            min_password_length: 4,
            require_uppercase: false,
            require_lowercase: false,
            require_digit: false,
            ..ValidatorConfig::default()
        });
        assert!(validator.password("password", "abcd").is_ok());
        assert!(validator.password("password", "abc").is_err());
    }

    #[test]
    fn the_mobile_shapes_a_form_produces_all_pass() {
        let validator = validator();

        for shape in [
            "13800138000",
            "+8613800138000",
            "86 138-0013-8000",
            "138 0013 8000",
        ] {
            assert!(validator.mobile("mobile", shape).is_ok(), "{shape}");
        }

        // Eleven digits starting with a one is the rule, so `12800138000` is *valid* by it.
        for bad in ["23800138000", "1380013800", "138001380001", "not a number"] {
            let error = validator.mobile("mobile", bad).unwrap_err();
            assert_eq!(error.code, code::MOBILE_INVALID, "{bad}");
        }
    }

    #[test]
    fn an_email_is_checked_loosely_on_purpose() {
        let validator = validator();

        for good in ["a@b.co", "first.last+tag@sub.example.com"] {
            assert!(validator.email("email", good).is_ok(), "{good}");
        }
        for bad in [
            "no-at-sign",
            "@example.com",
            "a@b",
            "a b@example.com",
            "a@.com",
        ] {
            assert!(validator.email("email", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn required_and_max_length_report_the_field() {
        let validator = validator();

        let error = validator.required("name", "   ").unwrap_err();
        assert_eq!(error.code, code::FIELD_INVALID);
        assert!(error.client_message().contains("name"));

        assert!(validator.max_length("name", "abc", 3).is_ok());
        assert!(validator.max_length("name", "abcd", 3).is_err());
        // Characters, not bytes: three Chinese characters are three, not nine.
        assert!(validator.max_length("name", "北京人", 3).is_ok());
    }

    #[test]
    fn a_rule_of_the_callers_own_fits_alongside_the_built_in_ones() {
        let validator = validator();
        let ok = validator.check(
            "code",
            "AB-12",
            |v| v.starts_with("AB"),
            "must start with AB",
        );
        assert!(ok.is_ok());

        let bad = validator.check(
            "code",
            "XY-12",
            |v| v.starts_with("AB"),
            "must start with AB",
        );
        assert!(bad.is_err());
    }

    #[test]
    fn an_error_serialises_with_its_code_and_field() {
        let error =
            ValidationError::new(code::MOBILE_INVALID, "that is not a mobile number").on("mobile");
        let json = serde_json::to_value(&error).unwrap();

        assert_eq!(json["code"], 2002);
        assert_eq!(json["field"], "mobile");
        assert!(json["message"].as_str().unwrap().contains("mobile number"));
    }

    #[test]
    fn a_validation_error_becomes_an_envelope_error() {
        let error = ValidationError::new(code::FIELD_INVALID, "required").on("name");
        let converted: Error = error.into();
        // The business code travels; the status stays 200, like every other business code.
        assert_eq!(converted.code, 2003);
        assert_eq!(converted.http_status(), 200);
        assert!(converted.client_message().contains("name"));
    }
}
