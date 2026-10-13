//! The framework's uniform response envelope.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::error::code;

/// The standard response envelope: `{"code":200,"data":...,"msg":"ok","type":""}`.
///
/// Every JSON response the framework emits is a `Message`, so clients can parse one
/// shape and check `code` without looking at the HTTP status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    /// HTTP status code, or a business code (1000+).
    pub code: i32,
    /// Payload. `null` when there is none.
    pub data: Value,
    /// Short status text. Serialised as `msg`.
    #[serde(rename = "msg")]
    pub message: String,
    /// Optional discriminator for clients that render different payload shapes.
    /// Serialised as `type`, which is a Rust keyword.
    #[serde(rename = "type")]
    pub kind: String,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            code: code::OK,
            data: Value::Null,
            message: String::from("ok"),
            kind: String::new(),
        }
    }
}

impl Message {
    /// Success with the default `"ok"` message and no payload.
    pub fn ok() -> Self {
        Self::default()
    }

    /// Success with a custom message.
    pub fn with_message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// Success carrying a payload. The payload must serialise to JSON.
    pub fn data(data: impl Into<Value>) -> Self {
        Self {
            data: data.into(),
            ..Self::default()
        }
    }

    /// Success carrying a payload tagged with a `type` discriminator.
    pub fn typed(kind: impl Into<String>, data: impl Into<Value>) -> Self {
        Self {
            kind: kind.into(),
            data: data.into(),
            ..Self::default()
        }
    }

    /// A response with an explicit code and payload.
    pub fn with_code(code: i32, data: impl Into<Value>) -> Self {
        Self {
            code,
            data: data.into(),
            ..Self::default()
        }
    }

    /// A `301` redirect to `url`.
    pub fn redirect(url: impl Into<String>) -> Self {
        Self {
            code: 301,
            data: json!(url.into()),
            message: String::from("redirect"),
            kind: String::new(),
        }
    }

    /// Reports whether this is a success envelope.
    pub fn is_ok(&self) -> bool {
        self.code == code::OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_shape_is_stable() {
        let json = serde_json::to_value(Message::ok()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"code": 200, "data": null, "msg": "ok", "type": ""})
        );
    }

    #[test]
    fn data_serialises_into_the_envelope() {
        let json = serde_json::to_value(Message::data(serde_json::json!({"id": 1}))).unwrap();
        assert_eq!(json["code"], 200);
        assert_eq!(json["data"]["id"], 1);
    }

    #[test]
    fn redirect_uses_301_and_the_url_as_data() {
        let msg = Message::redirect("/new");
        assert_eq!(msg.code, 301);
        assert_eq!(msg.data, serde_json::json!("/new"));
    }
}
